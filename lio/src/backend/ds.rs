#![allow(
  clippy::large_enum_variant,
  clippy::question_mark,
  clippy::undocumented_unsafe_blocks,
  clippy::unnecessary_cast,
  clippy::unnecessary_map_or,
  clippy::collapsible_if
)]

//! Deterministic simulation backend for higher-level tests.
//!
//! `DSBackend` never calls into the operating system. Instead it executes
//! operations against a small synthetic world:
//!
//! - a seeded pseudo-random scheduler controls completion delay and ordering
//! - a synthetic filesystem backs `openat`/`stat`/`mkdir`/`rename`/`link`
//! - synthetic file/socket resources are reused across later operations
//! - standard resources (`stdin`, `stdout`, `stderr`, `cwd`) are modeled as
//!   deterministic built-ins
//!
//! The goal is deterministic replay of backend variance and stateful behavior,
//! not syscall fidelity.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::fmt;
use std::io;
use std::mem;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;

use bumpalo::Bump;

use crate::api::resource::Resource;
#[cfg(test)]
use crate::backend::op::SendFlags;
use crate::backend::op::{
  self, DirEntryRef, FileStat, FileType, MsgRecv, MsgSend, Op, OpaqueDropFn,
  RawBuf, ReadDirResult, ReadFlags, RecvFlags, ShutdownHow, SockDomain,
  SockProto, SockType, SocketAddrBuf, SocketAddrFamily, UnlinkKind,
};
use crate::backend::{IoBackend, OpCompleted};
use crate::{Lio, install_global};

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[derive(Debug, Clone, Copy)]
pub struct DSConfig {
  pub seed: u64,
  pub max_delay_ticks: u8,
  /// Roughly 1 in `fault_every` successful-looking operations is turned into a
  /// legal error for that op kind. `0` disables injected faults.
  pub fault_every: u8,
  pub network_faults: DSNetworkFaults,
}

/// Transport-only network fault policy for [`DST`].
///
/// These faults are deterministic: the same seed, simulated tick sequence,
/// node topology, and workload produce the same network behavior on every run.
/// The policy is intentionally protocol-agnostic. It only controls transport
/// reachability between simulated nodes; higher-level effects such as leader
/// failover or retries emerge from the workload running on top of that network.
///
/// `DSBackend` ignores this setting. It only applies to [`DST`]'s synthetic
/// network sockets.
#[derive(Debug, Clone, Copy)]
pub enum DSNetworkFaults {
  /// Disable transport fault injection. `DST` behaves like a healthy network
  /// except for its normal deterministic delay and ordering rules.
  Off,
  /// Deterministic link instability between simulated nodes.
  ///
  /// For each unordered pair of nodes, `DST` reevaluates the link once per
  /// `check_every_ticks` tick bucket. If the seed-derived decision marks that
  /// bucket as down, the link stays unavailable for a deterministic duration in
  /// the range `min_down_ticks..=max_down_ticks`.
  ///
  /// The backend remains transport-only:
  /// - `connect` fails while the link is down
  /// - stream traffic in flight is turned into connection-reset style failure
  /// - datagrams are dropped while the link is down
  ///
  /// This gives workloads a replayable but non-scripted flaky network without
  /// encoding any application or protocol knowledge into `DST`.
  FlakyLinks {
    /// Number of simulated ticks in one health-check bucket.
    check_every_ticks: u64,
    /// Approximate percentage chance that a bucket transitions a link into a
    /// down state. Values above `100` are clamped.
    down_probability_pct: u8,
    /// Minimum number of ticks a downed link remains unavailable.
    min_down_ticks: u64,
    /// Maximum number of ticks a downed link remains unavailable.
    max_down_ticks: u64,
  },
}

/// Node-scoped transport fault policy for a [`DSTNode`].
///
/// These policies are deterministic. `DST` evaluates them from the simulator
/// seed, the node's stable identity, the current simulated time bucket, and the
/// policy parameters. They remain transport-only: they affect whether a node is
/// reachable on the simulated network, not any application or protocol state.
#[derive(Debug, Clone, Copy)]
pub enum DSNodeFaults {
  /// Disable node-scoped transport faults.
  Off,
  /// Temporarily isolate the node from the simulated network.
  ///
  /// While isolated, all traffic to and from the node fails at the transport
  /// layer. While healthy, `DST` reevaluates once per simulated second whether
  /// a new isolation episode starts. If the deterministic decision says
  /// "isolate", the outage lasts for a deterministic duration in
  /// `min_down_ticks..=max_down_ticks`.
  Isolation(NodeIsolation),
}

/// Deterministic node-isolation policy for [`DSNodeFaults::Isolation`].
///
/// Each field controls a transport-only aspect of the outage schedule.
///
/// `isolation_risk_pct_per_second` is the approximate percentage chance that a
/// new isolation episode starts during any simulated second while the node is
/// currently healthy. Durations are still expressed in simulator ticks.
///
/// For the same seed, node id, tick progression, and policy values, the same
/// node will isolate and heal at the same simulated times on every run.
#[derive(Debug, Clone, Copy)]
pub struct NodeIsolation {
  pub isolation_risk_pct_per_second: u8,
  pub min_down_ticks: u64,
  pub max_down_ticks: u64,
}

impl Default for DSConfig {
  fn default() -> Self {
    Self {
      seed: env_u64("LIO_DS_SEED").unwrap_or(0x4453_5f42_4143_4b45),
      max_delay_ticks: 3,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    }
  }
}

impl DSConfig {
  pub fn fault_every(mut self, each_op: u8) -> Self {
    self.fault_every = each_op;
    self
  }
  pub fn describe(self) -> String {
    let mut topology_rng = Prng::new(split_seed(self.seed, b"topology"));
    let scenario = DSBackend::build_scenario(&mut topology_rng);
    format!(
      "seed={} max_delay_ticks={} fault_every={} network_faults={:?} cwd={} readdir_root={} fs_nodes={} occupied_addrs={} occupied_unix_addrs={}",
      self.seed,
      self.max_delay_ticks,
      self.fault_every,
      self.network_faults,
      String::from_utf8_lossy(&scenario.cwd),
      String::from_utf8_lossy(&scenario.readdir_root),
      scenario.fs_nodes.len(),
      scenario.occupied_addr_keys.len(),
      scenario.occupied_unix_addr_keys.len(),
    )
  }
}

impl fmt::Display for DSConfig {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.describe())
  }
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

  fn range_u64(&mut self, upper_exclusive: u64) -> u64 {
    if upper_exclusive <= 1 { 0 } else { self.next_u64() % upper_exclusive }
  }

  fn range_usize(&mut self, upper_exclusive: usize) -> usize {
    self.range_u64(upper_exclusive as u64) as usize
  }

  fn shuffle<T>(&mut self, values: &mut [T]) {
    for i in (1..values.len()).rev() {
      let j = self.range_usize(i + 1);
      values.swap(i, j);
    }
  }
}

#[derive(Debug, Clone)]
enum ScheduledEvent {
  Complete {
    ready_at_tick: u64,
    id: u64,
    result: isize,
  },
  ConnectEstablished {
    ready_at_tick: u64,
    id: u64,
    listener_fd: i32,
    accepted_fd: i32,
  },
  StreamDelivered {
    ready_at_tick: u64,
    to_fd: i32,
    bytes: Vec<u8>,
  },
  SocketShutdownRead {
    ready_at_tick: u64,
    fd: i32,
  },
}

#[derive(Clone, Copy)]
struct ReadDirTargets {
  raw_buf: NonNull<u8>,
  raw_cap: usize,
  entries: NonNull<DirEntryRef>,
  entries_cap: usize,
  out: NonNull<ReadDirResult>,
  opaque: NonNull<*mut ()>,
  opaque_drop: NonNull<Option<OpaqueDropFn>>,
}

#[derive(Debug, Clone)]
enum SimNode {
  Directory { mode: u32 },
  File { bytes: Vec<u8>, mode: u32 },
  Symlink { target: Vec<u8>, mode: u32 },
}

#[derive(Debug, Clone)]
struct SimFileHandle {
  path: Vec<u8>,
  cursor: usize,
  readable: bool,
  writable: bool,
  append: bool,
}

#[derive(Debug, Clone)]
struct SimSocketHandle {
  domain: SockDomain,
  ty: SockType,
  proto: SockProto,
  inbox: VecDeque<u8>,
  bound: Option<SocketAddrBuf>,
  peer_addr: Option<SocketAddrBuf>,
  listening: bool,
  backlog: i32,
  shutdown_read: bool,
  shutdown_write: bool,
  pending_accept: VecDeque<i32>,
  peer: Option<i32>,
}

#[derive(Debug, Clone)]
enum SimResource {
  File(SimFileHandle),
  Socket(SimSocketHandle),
}

#[derive(Debug, Clone)]
struct DSScenario {
  cwd: Vec<u8>,
  readdir_root: Vec<u8>,
  short_io_zero_mod: u8,
  fs_nodes: Vec<(Vec<u8>, SimNode)>,
  occupied_addr_keys: Vec<Vec<u8>>,
  occupied_unix_addr_keys: Vec<Vec<u8>>,
}

#[derive(Debug)]
struct DSWorld {
  tick: u64,
  next_resource_id: i32,
  next_pid: i32,
  stdin_cursor: u64,
  fs: BTreeMap<Vec<u8>, SimNode>,
  resources: BTreeMap<i32, SimResource>,
  bound_listeners: BTreeMap<Vec<u8>, i32>,
  occupied_addr_keys: Vec<Vec<u8>>,
  events: Vec<ScheduledEvent>,
  pending: Vec<(u64, Op)>,
}

#[derive(Debug)]
pub struct DSBackend {
  cap: usize,
  initialized: bool,
  queued: Vec<(u64, Op)>,
  ready: Vec<OpCompleted>,
  schedule_rng: Prng,
  fault_rng: Prng,
  payload_rng: Prng,
  topology_rng: Prng,
  resource_rng: Prng,
  config: DSConfig,
  scenario: DSScenario,
  world: DSWorld,
  trace: VecDeque<String>,
}

enum PendingResult<T> {
  Ready(T),
  Pending,
}

enum PendingAction {
  Complete(isize),
  KeepPending(Op),
}

enum OpAction {
  Complete(isize),
  Pending(Op),
  StartConnect { accepted_fd: i32, listener_fd: i32, ready_at_tick: u64 },
}

#[derive(Debug, Clone)]
pub struct DSTraceSnapshot {
  pub config: DSConfig,
  pub scenario: String,
  pub trace: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DSProgressSnapshot {
  pub tick: u64,
  pub queued: usize,
  pub pending: usize,
  pub scheduled_events: usize,
  pub ready: usize,
  pub resources: usize,
  pub trace_len: usize,
}

thread_local! {
  static LAST_DS_TRACE: RefCell<Option<DSTraceSnapshot>> = const { RefCell::new(None) };
}

pub fn last_ds_trace_snapshot() -> Option<DSTraceSnapshot> {
  LAST_DS_TRACE.with(|slot| slot.borrow().clone())
}

const SYNTHETIC_RESOURCE_BASE: i32 = 1_000_000;
const SYNTHETIC_PID_BASE: i32 = 2_000_000;
const DST_SOCKET_RESOURCE_BASE: i32 = 10_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DSTNodeId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DSTSocketRef {
  node_id: DSTNodeId,
  fd: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DSTSocketState {
  Created,
  Bound,
  Listening,
  Connecting,
  Connected,
  Closed,
}

#[derive(Debug, Clone)]
enum DSTRecvQueue {
  Stream(VecDeque<u8>),
  Datagram(VecDeque<Vec<u8>>),
}

#[derive(Debug, Clone)]
enum DSTEvent {
  ConnectEstablished {
    ready_at_tick: u64,
    source_node: DSTNodeId,
    source_seq: u64,
    connect_id: u64,
    client: DSTSocketRef,
    listener: DSTSocketRef,
    server_fd: i32,
    server_peer_addr: SocketAddrBuf,
  },
  StreamDelivered {
    ready_at_tick: u64,
    source_node: DSTNodeId,
    source_seq: u64,
    from: DSTSocketRef,
    to: DSTSocketRef,
    bytes: Vec<u8>,
  },
  DatagramDelivered {
    ready_at_tick: u64,
    source_node: DSTNodeId,
    source_seq: u64,
    from: DSTSocketRef,
    from_addr: SocketAddrBuf,
    to: DSTSocketRef,
    packet: Vec<u8>,
  },
  SocketShutdownRead {
    ready_at_tick: u64,
    source_node: DSTNodeId,
    source_seq: u64,
    to: DSTSocketRef,
  },
  #[allow(dead_code)]
  SocketReset {
    ready_at_tick: u64,
    source_node: DSTNodeId,
    source_seq: u64,
    to: DSTSocketRef,
    errno: i32,
  },
}

#[derive(Debug, Default)]
struct DueEventBucket {
  by_source: BTreeMap<DSTNodeId, VecDeque<DSTEvent>>,
}

impl DSTEvent {
  fn ready_at_tick(&self) -> u64 {
    match self {
      Self::ConnectEstablished { ready_at_tick, .. }
      | Self::StreamDelivered { ready_at_tick, .. }
      | Self::DatagramDelivered { ready_at_tick, .. }
      | Self::SocketShutdownRead { ready_at_tick, .. }
      | Self::SocketReset { ready_at_tick, .. } => *ready_at_tick,
    }
  }

  fn source_node(&self) -> DSTNodeId {
    match self {
      Self::ConnectEstablished { source_node, .. }
      | Self::StreamDelivered { source_node, .. }
      | Self::DatagramDelivered { source_node, .. }
      | Self::SocketShutdownRead { source_node, .. }
      | Self::SocketReset { source_node, .. } => *source_node,
    }
  }

  fn source_seq(&self) -> u64 {
    match self {
      Self::ConnectEstablished { source_seq, .. }
      | Self::StreamDelivered { source_seq, .. }
      | Self::DatagramDelivered { source_seq, .. }
      | Self::SocketShutdownRead { source_seq, .. }
      | Self::SocketReset { source_seq, .. } => *source_seq,
    }
  }

  fn kind_rank(&self) -> u8 {
    match self {
      Self::ConnectEstablished { .. } => 0,
      Self::StreamDelivered { .. } => 1,
      Self::DatagramDelivered { .. } => 2,
      Self::SocketShutdownRead { .. } => 3,
      Self::SocketReset { .. } => 4,
    }
  }

  fn stable_tie_break(&self) -> (u64, u8, i64, i64) {
    match self {
      Self::ConnectEstablished { connect_id, listener, server_fd, .. } => {
        (*connect_id, self.kind_rank(), listener.fd as i64, *server_fd as i64)
      }
      Self::StreamDelivered { to, bytes, .. } => (
        bytes.len() as u64,
        self.kind_rank(),
        to.node_id.0 as i64,
        to.fd as i64,
      ),
      Self::DatagramDelivered { to, packet, .. } => (
        packet.len() as u64,
        self.kind_rank(),
        to.node_id.0 as i64,
        to.fd as i64,
      ),
      Self::SocketShutdownRead { to, .. } => {
        (0, self.kind_rank(), to.node_id.0 as i64, to.fd as i64)
      }
      Self::SocketReset { to, errno, .. } => {
        (*errno as u64, self.kind_rank(), to.node_id.0 as i64, to.fd as i64)
      }
    }
  }
}

impl DueEventBucket {
  fn insert(&mut self, event: DSTEvent) {
    let source_node = event.source_node();
    let event_key = (event.source_seq(), event.stable_tie_break());
    let queue = self.by_source.entry(source_node).or_default();
    let insert_at = queue
      .iter()
      .position(|existing| {
        (existing.source_seq(), existing.stable_tie_break()) > event_key
      })
      .unwrap_or(queue.len());
    queue.insert(insert_at, event);
  }

  fn pop_next_event(
    &mut self,
    inner: &DSTInner,
    ready_at_tick: u64,
    arbitration_round: u64,
  ) -> Option<DSTEvent> {
    let mut chosen_source = None;
    let mut chosen_source_rank = None;
    for &source_node in self.by_source.keys() {
      let rank =
        inner.delivery_rank(ready_at_tick, arbitration_round, source_node);
      let key = (rank, source_node);
      if chosen_source_rank.map_or(true, |best| key < best) {
        chosen_source_rank = Some(key);
        chosen_source = Some(source_node);
      }
    }

    let chosen_source = chosen_source?;
    let queue = self
      .by_source
      .get_mut(&chosen_source)
      .expect("chosen due-event source should exist");
    let event = queue
      .pop_front()
      .expect("chosen due-event source should have a queued event");
    if queue.is_empty() {
      self.by_source.remove(&chosen_source);
    }
    Some(event)
  }

  fn is_empty(&self) -> bool {
    self.by_source.is_empty()
  }
}

#[derive(Debug)]
struct DSTNodeState {
  initialized: bool,
  cap: usize,
  next_fd: i32,
  next_network_seq: u64,
  sockets: BTreeMap<i32, DSTSocketHandle>,
  outgoing: VecDeque<(u64, Op)>,
  pending: Vec<(u64, Op)>,
  ready: Vec<OpCompleted>,
}

impl Default for DSTNodeState {
  fn default() -> Self {
    Self {
      initialized: false,
      cap: 0,
      next_fd: DST_SOCKET_RESOURCE_BASE,
      next_network_seq: 0,
      sockets: BTreeMap::new(),
      outgoing: VecDeque::new(),
      pending: Vec::new(),
      ready: Vec::new(),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DSTLinkKey {
  a: DSTNodeId,
  b: DSTNodeId,
}

impl DSTLinkKey {
  fn between(left: DSTNodeId, right: DSTNodeId) -> Self {
    if left <= right {
      Self { a: left, b: right }
    } else {
      Self { a: right, b: left }
    }
  }
}

#[derive(Debug, Clone, Copy)]
struct DSTLinkFaultState {
  evaluated_bucket: u64,
  down_until_tick: u64,
}

#[derive(Debug, Clone, Copy)]
struct DSTNodeFaultRuntime {
  policy: DSNodeFaults,
  evaluated_bucket: u64,
  isolated_until_tick: u64,
}

#[derive(Debug)]
struct DSTInner {
  tick: u64,
  next_node_id: u64,
  schedule_seed: u64,
  max_delay_ticks: u8,
  network_faults: DSNetworkFaults,
  occupied_addr_keys: Vec<Vec<u8>>,
  bound: BTreeMap<Vec<u8>, DSTSocketRef>,
  listeners: BTreeMap<Vec<u8>, DSTSocketRef>,
  nodes: Vec<Rc<RefCell<DSTNodeState>>>,
  events: BTreeMap<u64, DueEventBucket>,
  active_outgoing_nodes: BTreeSet<DSTNodeId>,
  node_faults: BTreeMap<DSTNodeId, DSTNodeFaultRuntime>,
  link_faults: BTreeMap<DSTLinkKey, DSTLinkFaultState>,
}

#[derive(Debug, Clone)]
struct DSTSocketHandle {
  domain: SockDomain,
  ty: SockType,
  proto: SockProto,
  state: DSTSocketState,
  recv_queue: DSTRecvQueue,
  next_stream_ready_at_tick: u64,
  bound: Option<SocketAddrBuf>,
  peer_addr: Option<SocketAddrBuf>,
  backlog: i32,
  backlog_reserved: usize,
  local_shutdown_read: bool,
  local_shutdown_write: bool,
  peer_write_closed: bool,
  peer_reset_error: Option<i32>,
  pending_accept: VecDeque<i32>,
  peer: Option<DSTSocketRef>,
}

impl DSTInner {
  fn new(config: DSConfig) -> Self {
    let mut topology_rng = Prng::new(split_seed(config.seed, b"topology"));
    let scenario = DSBackend::build_scenario(&mut topology_rng);
    let mut occupied_addr_keys = scenario.occupied_addr_keys;
    occupied_addr_keys.extend(scenario.occupied_unix_addr_keys);
    Self {
      tick: 0,
      next_node_id: 0,
      schedule_seed: split_seed(config.seed, b"dst-schedule"),
      max_delay_ticks: config.max_delay_ticks,
      network_faults: config.network_faults,
      occupied_addr_keys,
      bound: BTreeMap::new(),
      listeners: BTreeMap::new(),
      nodes: Vec::new(),
      events: BTreeMap::new(),
      active_outgoing_nodes: BTreeSet::new(),
      node_faults: BTreeMap::new(),
      link_faults: BTreeMap::new(),
    }
  }

  fn enqueue_event(&mut self, event: DSTEvent) {
    self.events.entry(event.ready_at_tick()).or_default().insert(event);
  }

  fn take_due_event_bucket(&mut self) -> Option<(u64, DueEventBucket)> {
    let Some((&ready_at_tick, _)) = self.events.first_key_value() else {
      return None;
    };
    if ready_at_tick > self.tick {
      return None;
    }
    self.events.pop_first()
  }

  fn event_delay_ticks_for(
    &self,
    source_node: DSTNodeId,
    source_seq: u64,
    kind: u8,
  ) -> u64 {
    if self.max_delay_ticks == 0 {
      return 1;
    }
    let delay = deterministic_mix(
      self.schedule_seed,
      &[self.tick, source_node.0, source_seq, kind as u64],
    ) % ((self.max_delay_ticks as u64) + 1);
    delay.saturating_add(1)
  }

  fn delivery_rank(
    &self,
    ready_at_tick: u64,
    arbitration_round: u64,
    source_node: DSTNodeId,
  ) -> u64 {
    deterministic_mix(
      self.schedule_seed ^ 0xa5a5_a5a5_a5a5_a5a5,
      &[ready_at_tick, arbitration_round, source_node.0],
    )
  }

  fn node(&self, node_id: DSTNodeId) -> Rc<RefCell<DSTNodeState>> {
    self
      .nodes
      .get(node_id.0 as usize)
      .cloned()
      .expect("DST node should exist for backend")
  }

  fn set_node_faults(&mut self, node_id: DSTNodeId, policy: DSNodeFaults) {
    let state =
      self.node_faults.entry(node_id).or_insert(DSTNodeFaultRuntime {
        policy: DSNodeFaults::Off,
        evaluated_bucket: u64::MAX,
        isolated_until_tick: 0,
      });
    state.policy = policy;
    state.evaluated_bucket = u64::MAX;
    state.isolated_until_tick = 0;
  }

  fn node_is_available(&mut self, node_id: DSTNodeId) -> bool {
    let state =
      self.node_faults.entry(node_id).or_insert(DSTNodeFaultRuntime {
        policy: DSNodeFaults::Off,
        evaluated_bucket: u64::MAX,
        isolated_until_tick: 0,
      });
    match state.policy {
      DSNodeFaults::Off => true,
      DSNodeFaults::Isolation(policy) => {
        if self.tick < state.isolated_until_tick {
          return false;
        }

        let bucket = self.tick / 1_000;
        if bucket == 0 {
          state.evaluated_bucket = bucket;
          state.isolated_until_tick = 0;
          return true;
        }
        if state.evaluated_bucket == bucket {
          return true;
        }
        state.evaluated_bucket = bucket;

        let pct = policy.isolation_risk_pct_per_second.min(100) as u64;
        if pct == 0 {
          state.isolated_until_tick = 0;
          return true;
        }

        let roll = deterministic_mix(
          self.schedule_seed ^ 0x1a50_1a7e_600d_0001,
          &[bucket, node_id.0],
        ) % 100;
        if roll >= pct {
          state.isolated_until_tick = 0;
          return true;
        }

        let min_ticks = policy.min_down_ticks.max(1);
        let max_ticks = policy.max_down_ticks.max(min_ticks);
        let span = max_ticks - min_ticks + 1;
        let duration = min_ticks
          + (deterministic_mix(
            self.schedule_seed ^ 0x1a50_1a7e_600d_0002,
            &[bucket, node_id.0],
          ) % span);
        state.isolated_until_tick = self.tick.saturating_add(duration);
        false
      }
    }
  }

  fn link_is_available(&mut self, left: DSTNodeId, right: DSTNodeId) -> bool {
    if left == right {
      return true;
    }
    if !self.node_is_available(left) || !self.node_is_available(right) {
      return false;
    }
    match self.network_faults {
      DSNetworkFaults::Off => true,
      DSNetworkFaults::FlakyLinks {
        check_every_ticks,
        down_probability_pct,
        min_down_ticks,
        max_down_ticks,
      } => {
        let bucket_span = check_every_ticks.max(1);
        let bucket = self.tick / bucket_span;
        let key = DSTLinkKey::between(left, right);
        let state = self.link_faults.entry(key).or_insert(DSTLinkFaultState {
          evaluated_bucket: u64::MAX,
          down_until_tick: 0,
        });
        if self.tick < state.down_until_tick {
          return false;
        }
        if state.evaluated_bucket == bucket {
          return true;
        }
        state.evaluated_bucket = bucket;

        let pct = down_probability_pct.min(100) as u64;
        if pct == 0 {
          state.down_until_tick = 0;
          return true;
        }

        let roll = deterministic_mix(
          self.schedule_seed ^ 0xd157_fa17_5eed_1234,
          &[bucket, key.a.0, key.b.0],
        ) % 100;
        if roll >= pct {
          state.down_until_tick = 0;
          return true;
        }

        let min_ticks = min_down_ticks.max(1);
        let max_ticks = max_down_ticks.max(min_ticks);
        let span = max_ticks - min_ticks + 1;
        let duration = min_ticks
          + (deterministic_mix(
            self.schedule_seed ^ 0x51ab_1e77_d00d_0001,
            &[bucket, key.a.0, key.b.0, self.tick],
          ) % span);
        state.down_until_tick = self.tick.saturating_add(duration);
        false
      }
    }
  }
}

impl DSTSocketHandle {
  fn new(domain: SockDomain, ty: SockType, proto: SockProto) -> Self {
    Self {
      domain,
      ty,
      proto,
      state: DSTSocketState::Created,
      recv_queue: match ty {
        SockType::DGRAM => DSTRecvQueue::Datagram(VecDeque::new()),
        _ => DSTRecvQueue::Stream(VecDeque::new()),
      },
      next_stream_ready_at_tick: 0,
      bound: None,
      peer_addr: None,
      backlog: 0,
      backlog_reserved: 0,
      local_shutdown_read: false,
      local_shutdown_write: false,
      peer_write_closed: false,
      peer_reset_error: None,
      pending_accept: VecDeque::new(),
      peer: None,
    }
  }

  fn can_receive_pending(&self) -> bool {
    !matches!(self.state, DSTSocketState::Closed)
      && matches!(
        self.state,
        DSTSocketState::Bound
          | DSTSocketState::Listening
          | DSTSocketState::Connected
          | DSTSocketState::Connecting
      )
  }
}

pub struct DST {
  config: DSConfig,
  inner: Rc<RefCell<DSTInner>>,
  runners: Vec<DSTNodeRunner>,
  runner_index_by_node: BTreeMap<DSTNodeId, usize>,
  ready_runners: VecDeque<usize>,
}

#[derive(Clone)]
pub struct DSTNode {
  inner: Rc<RefCell<DSTInner>>,
  node_id: DSTNodeId,
}

#[derive(Debug)]
struct DSTBackend {
  local: Option<DSBackend>,
  node_id: DSTNodeId,
  node: Rc<RefCell<DSTNodeState>>,
  inner: Rc<RefCell<DSTInner>>,
  queued: Vec<(u64, Op)>,
}

struct DSTNodeRunner {
  lio: Lio,
  queued_for_pump: bool,
  pump: Box<dyn FnMut(Lio) -> io::Result<()>>,
}

fn split_seed(root: u64, label: &[u8]) -> u64 {
  let mut hash = root ^ 0x9e37_79b9_7f4a_7c15;
  for &byte in label {
    hash ^= byte as u64;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 31;
  }
  if hash == 0 { 1 } else { hash }
}

fn deterministic_mix(seed: u64, values: &[u64]) -> u64 {
  let mut hash = seed ^ 0x517c_c1b7_2722_0a95;
  for &value in values {
    hash ^= value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    hash = hash.rotate_left(27).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 31;
  }
  if hash == 0 { 1 } else { hash }
}

fn env_u64(name: &str) -> Option<u64> {
  let value = env::var(name).ok()?;
  if let Some(hex) =
    value.strip_prefix("0x").or_else(|| value.strip_prefix("0X"))
  {
    u64::from_str_radix(hex, 16).ok()
  } else {
    value.parse().ok()
  }
}

impl DSTNode {
  pub fn id(&self) -> u64 {
    self.node_id.0
  }

  pub fn set_network_faults(&self, faults: DSNodeFaults) {
    self.inner.borrow_mut().set_node_faults(self.node_id, faults);
  }

  pub fn clear_network_faults(&self) {
    self.set_network_faults(DSNodeFaults::Off);
  }
}

impl Default for DSBackend {
  fn default() -> Self {
    Self::new()
  }
}

impl DSBackend {
  pub fn new() -> Self {
    Self::with_config(DSConfig::default())
  }

  pub fn with_seed(seed: u64) -> Self {
    Self::with_config(DSConfig { seed, ..DSConfig::default() })
  }

  pub fn with_config(config: DSConfig) -> Self {
    let mut topology_rng = Prng::new(split_seed(config.seed, b"topology"));
    let scenario = Self::build_scenario(&mut topology_rng);
    let mut backend = Self {
      cap: 0,
      initialized: false,
      queued: Vec::new(),
      ready: Vec::new(),
      schedule_rng: Prng::new(split_seed(config.seed, b"schedule")),
      fault_rng: Prng::new(split_seed(config.seed, b"faults")),
      payload_rng: Prng::new(split_seed(config.seed, b"payloads")),
      topology_rng,
      resource_rng: Prng::new(split_seed(config.seed, b"resources")),
      config,
      scenario,
      world: DSWorld {
        tick: 0,
        next_resource_id: SYNTHETIC_RESOURCE_BASE,
        next_pid: SYNTHETIC_PID_BASE,
        stdin_cursor: 0,
        fs: BTreeMap::new(),
        resources: BTreeMap::new(),
        bound_listeners: BTreeMap::new(),
        occupied_addr_keys: Vec::new(),
        events: Vec::new(),
        pending: Vec::new(),
      },
      trace: VecDeque::new(),
    };
    backend.reset_world();
    backend
  }

  pub fn progress_snapshot(&self) -> DSProgressSnapshot {
    DSProgressSnapshot {
      tick: self.world.tick,
      queued: self.queued.len(),
      pending: self.world.pending.len(),
      scheduled_events: self.world.events.len(),
      ready: self.ready.len(),
      resources: self.world.resources.len(),
      trace_len: self.trace.len(),
    }
  }

  pub fn is_quiescent(&self) -> bool {
    self.queued.is_empty()
      && self.world.pending.is_empty()
      && self.world.events.is_empty()
      && self.ready.is_empty()
  }

  pub fn has_pending_work(&self) -> bool {
    !self.queued.is_empty()
      || !self.world.pending.is_empty()
      || !self.world.events.is_empty()
  }

  fn assert_initialized(&self) {
    assert!(self.initialized, "DSBackend not initialized");
  }

  fn assert_capacity(&self) {
    assert!(
      self.queued.len() + self.world.events.len() + self.ready.len() < self.cap,
      "IoBackend capacity exceeded"
    );
  }

  fn build_scenario(rng: &mut Prng) -> DSScenario {
    let cwd_choices: [&[u8]; 4] =
      [b"/home/sim", b"/tmp", b"/var/tmp", b"/usr/share/doc"];
    let readdir_choices: [&[u8]; 4] =
      [b"/tmp", b"/etc", b"/usr/bin", b"/var/log"];
    let cwd = cwd_choices[rng.range_usize(cwd_choices.len())].to_vec();
    let readdir_root =
      readdir_choices[rng.range_usize(readdir_choices.len())].to_vec();

    let mut fs_nodes = vec![
      (b"/".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/bin".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/dev".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/etc".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/home".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/home/sim".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/lib".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/tmp".to_vec(), SimNode::Directory { mode: 0o777 }),
      (b"/usr".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/usr/bin".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/usr/share".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/usr/share/doc".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/var".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/var/log".to_vec(), SimNode::Directory { mode: 0o755 }),
      (b"/var/tmp".to_vec(), SimNode::Directory { mode: 0o777 }),
      (
        b"/bin/echo".to_vec(),
        SimNode::File { bytes: b"synthetic-echo".to_vec(), mode: 0o755 },
      ),
      (
        b"/etc/hosts".to_vec(),
        SimNode::File { bytes: b"127.0.0.1 localhost\n".to_vec(), mode: 0o644 },
      ),
      (
        b"/etc/resolv.conf".to_vec(),
        SimNode::File {
          bytes: b"nameserver 127.0.0.1\n".to_vec(),
          mode: 0o644,
        },
      ),
      (
        b"/var/log/ds.log".to_vec(),
        SimNode::File { bytes: b"boot ok\n".to_vec(), mode: 0o644 },
      ),
      (
        b"/usr/share/doc/readme".to_vec(),
        SimNode::File {
          bytes: b"deterministic simulator\n".to_vec(),
          mode: 0o644,
        },
      ),
      (
        b"/etc/mtab".to_vec(),
        SimNode::Symlink { target: b"/proc/mounts".to_vec(), mode: 0o777 },
      ),
    ];

    for idx in 0..(1 + rng.range_usize(4)) {
      let path = format!("/tmp/ds-{:02x}-{:02x}", idx, rng.next_u64() as u8)
        .into_bytes();
      fs_nodes.push((
        path,
        SimNode::File {
          bytes: format!("tmp-file-{:08x}\n", rng.next_u64() as u32)
            .into_bytes(),
          mode: 0o600 + rng.range_usize(0o77) as u32,
        },
      ));
    }

    for idx in 0..(1 + rng.range_usize(3)) {
      let leaf = format!("tool-{:02x}", idx).into_bytes();
      let mut path = b"/usr/bin/".to_vec();
      path.extend_from_slice(&leaf);
      fs_nodes.push((
        path,
        SimNode::File {
          bytes: format!("#!/bin/sh\n# {}\n", idx).into_bytes(),
          mode: 0o755,
        },
      ));
    }

    for idx in 0..(1 + rng.range_usize(3)) {
      let path = format!("/home/sim/note-{:02x}.txt", idx).into_bytes();
      fs_nodes.push((
        path,
        SimNode::File {
          bytes: format!("seeded-note-{:08x}\n", rng.next_u64() as u32)
            .into_bytes(),
          mode: 0o644,
        },
      ));
    }

    let occupied_addr_keys = (0..rng.range_usize(3))
      .map(|idx| {
        let host = 10 + idx as u8;
        let port = (20_000 + rng.range_usize(20_000) as u16).to_be();
        let mut key = b"ipv4:".to_vec();
        key.extend_from_slice(&[127, 0, 0, host]);
        key.extend_from_slice(&port.to_be_bytes());
        key
      })
      .collect();

    let occupied_unix_addr_keys = (0..rng.range_usize(3))
      .map(|idx| format!("unix:/tmp/ds-busy-{:02x}.sock", idx).into_bytes())
      .collect();

    DSScenario {
      cwd,
      readdir_root,
      short_io_zero_mod: 4 + rng.range_usize(8) as u8,
      fs_nodes,
      occupied_addr_keys,
      occupied_unix_addr_keys,
    }
  }

  fn reset_world(&mut self) {
    self.schedule_rng = Prng::new(split_seed(self.config.seed, b"schedule"));
    self.fault_rng = Prng::new(split_seed(self.config.seed, b"faults"));
    self.payload_rng = Prng::new(split_seed(self.config.seed, b"payloads"));
    self.topology_rng = Prng::new(split_seed(self.config.seed, b"topology"));
    self.resource_rng = Prng::new(split_seed(self.config.seed, b"resources"));
    self.scenario = Self::build_scenario(&mut self.topology_rng);
    self.world.tick = 0;
    self.world.next_resource_id = SYNTHETIC_RESOURCE_BASE;
    self.world.next_pid = SYNTHETIC_PID_BASE;
    self.world.stdin_cursor = 0;
    self.world.fs.clear();
    self.world.resources.clear();
    self.world.bound_listeners.clear();
    self.world.occupied_addr_keys = self.scenario.occupied_addr_keys.clone();
    self
      .world
      .occupied_addr_keys
      .extend(self.scenario.occupied_unix_addr_keys.clone());
    self.world.events.clear();
    self.world.pending.clear();
    self.trace.clear();
    self.publish_trace_snapshot();
    self.bootstrap_fs();
  }

  fn bootstrap_fs(&mut self) {
    for (path, node) in &self.scenario.fs_nodes {
      self.world.fs.insert(path.clone(), node.clone());
    }
  }

  fn allocate_resource_id(&mut self) -> i32 {
    let id = self.world.next_resource_id;
    self.world.next_resource_id = self.world.next_resource_id.saturating_add(1);
    id
  }

  fn allocate_pid(&mut self) -> i32 {
    let pid = self.world.next_pid;
    self.world.next_pid = self.world.next_pid.saturating_add(1);
    pid
  }

  fn delay_ticks(&mut self) -> u64 {
    if self.config.max_delay_ticks == 0 {
      0
    } else {
      self.schedule_rng.range_u64((self.config.max_delay_ticks as u64) + 1)
    }
  }

  fn event_delay_ticks(&mut self) -> u64 {
    self.delay_ticks().saturating_add(1)
  }

  fn schedule_completion(&mut self, id: u64, result: isize) {
    let ready_at_tick = self.world.tick + self.delay_ticks();
    self.record_trace(format!(
      "tick={} schedule-complete id={} result={} at={}",
      self.world.tick, id, result, ready_at_tick
    ));
    if ready_at_tick == self.world.tick {
      self.ready.push(OpCompleted::new(id, result));
    } else {
      self.world.events.push(ScheduledEvent::Complete {
        ready_at_tick,
        id,
        result,
      });
    }
  }

  fn advance_tick(&mut self) {
    self.world.tick = self.world.tick.saturating_add(1);
    self.record_trace(format!("tick={} advance", self.world.tick));
    let mut idx = 0usize;
    while idx < self.world.events.len() {
      let due = match self.world.events[idx] {
        ScheduledEvent::Complete { ready_at_tick, .. }
        | ScheduledEvent::ConnectEstablished { ready_at_tick, .. }
        | ScheduledEvent::StreamDelivered { ready_at_tick, .. }
        | ScheduledEvent::SocketShutdownRead { ready_at_tick, .. } => {
          ready_at_tick <= self.world.tick
        }
      };
      if due {
        let done = self.world.events.swap_remove(idx);
        match done {
          ScheduledEvent::Complete { id, result, .. } => {
            self.record_trace(format!(
              "tick={} deliver-complete id={} result={}",
              self.world.tick, id, result
            ));
            self.ready.push(OpCompleted::new(id, result));
          }
          ScheduledEvent::ConnectEstablished {
            id,
            listener_fd,
            accepted_fd,
            ..
          } => {
            self.record_trace(format!(
              "tick={} connect-established id={} listener_fd={} accepted_fd={}",
              self.world.tick, id, listener_fd, accepted_fd
            ));
            if let Some(SimResource::Socket(listener)) =
              self.world.resources.get_mut(&listener_fd)
            {
              listener.pending_accept.push_back(accepted_fd);
            }
            self.ready.push(OpCompleted::new(id, 0));
          }
          ScheduledEvent::StreamDelivered { to_fd, bytes, .. } => {
            self.record_trace(format!(
              "tick={} stream-delivered fd={} len={}",
              self.world.tick,
              to_fd,
              bytes.len()
            ));
            if let Some(SimResource::Socket(socket)) =
              self.world.resources.get_mut(&to_fd)
            {
              socket.inbox.extend(bytes);
            }
          }
          ScheduledEvent::SocketShutdownRead { fd, .. } => {
            self.record_trace(format!(
              "tick={} socket-shutdown-read fd={}",
              self.world.tick, fd
            ));
            if let Some(SimResource::Socket(socket)) =
              self.world.resources.get_mut(&fd)
            {
              socket.shutdown_read = true;
            }
          }
        }
      } else {
        idx += 1;
      }
    }
    self.poll_pending_ops();
    self.schedule_rng.shuffle(&mut self.ready);
  }

  fn record_trace(&mut self, line: String) {
    const TRACE_LIMIT: usize = 256;
    if self.trace.len() == TRACE_LIMIT {
      self.trace.pop_front();
    }
    self.trace.push_back(line);
    self.publish_trace_snapshot();
  }

  fn publish_trace_snapshot(&self) {
    let snapshot = DSTraceSnapshot {
      config: self.config,
      scenario: self.config.describe(),
      trace: self.trace.iter().cloned().collect(),
    };
    LAST_DS_TRACE.with(|slot| {
      *slot.borrow_mut() = Some(snapshot);
    });
  }

  fn maybe_inject_fault(&mut self, op: &Op) -> Option<isize> {
    if self.config.fault_every == 0 || matches!(op, Op::Nop) {
      return None;
    }
    if self.fault_rng.range_u64(self.config.fault_every as u64) != 0 {
      return None;
    }
    Some(-(self.op_errno(op) as isize))
  }

  fn op_errno(&mut self, op: &Op) -> i32 {
    let choices: &[i32] = match op {
      Op::Read { .. } => &[libc::EIO, libc::EBADF, libc::EINVAL, libc::EAGAIN],
      Op::Write { fd, .. } => {
        match self.world.resources.get(&Self::resource_key(fd)) {
          Some(SimResource::Socket(_)) => {
            &[libc::EAGAIN, libc::ECONNRESET, libc::EPIPE, libc::EBADF]
          }
          _ => {
            &[libc::EIO, libc::EBADF, libc::EINVAL, libc::ENOSPC, libc::EFBIG]
          }
        }
      }
      Op::Recv { .. } => &[libc::ECONNRESET, libc::EAGAIN, libc::EBADF],
      Op::Send { .. } => &[libc::ECONNRESET, libc::EPIPE, libc::EBADF],
      Op::Accept { .. } => &[libc::ECONNABORTED, libc::EAGAIN, libc::EMFILE],
      Op::Connect { .. } => {
        &[libc::ECONNREFUSED, libc::ETIMEDOUT, libc::ENOENT]
      }
      Op::OpenAt { .. } => &[libc::ENOENT, libc::EACCES],
      Op::UnlinkAt { .. } => &[libc::ENOENT, libc::EACCES],
      Op::RenameAt { .. } => &[libc::ENOENT, libc::EACCES],
      Op::MkdirAt { .. } => &[libc::EEXIST, libc::EACCES],
      Op::LinkAt { .. } => &[libc::ENOENT, libc::EEXIST],
      Op::ReadlinkAt { .. } => &[libc::ENOENT, libc::EINVAL],
      Op::GetCwd { .. } => &[libc::ERANGE],
      Op::Spawn { .. } => &[libc::ENOENT, libc::EACCES],
      Op::Socket { .. } => &[libc::EMFILE, libc::ENFILE],
      Op::Bind { .. } => &[libc::EADDRINUSE, libc::EACCES],
      Op::Listen { .. } => &[libc::EINVAL, libc::ENOTSOCK],
      Op::Shutdown { .. } => &[libc::ENOTSOCK, libc::EINVAL],
      Op::Fsync { .. } => &[libc::EBADF, libc::EINVAL, libc::EIO],
      Op::Stat { target, .. } => match target {
        op::StatTarget::Path { .. } => &[libc::ENOENT, libc::EACCES],
        op::StatTarget::Fd { .. } => &[libc::EBADF],
      },
      Op::ReadDir { .. } => &[libc::EBADF, libc::EINVAL],
      Op::Nop => &[libc::EIO],
    };
    choices[self.fault_rng.range_usize(choices.len())]
  }

  #[cfg(unix)]
  fn resource_key(resource: &Resource) -> i32 {
    resource.as_raw_fd()
  }

  #[cfg(windows)]
  fn resource_key(resource: &Resource) -> i32 {
    resource.as_raw_handle() as isize as i32
  }

  fn is_builtin_stdin(raw: i32) -> bool {
    raw == 0
  }

  fn is_builtin_stdout(raw: i32) -> bool {
    raw == 1 || raw == 2
  }

  #[cfg(unix)]
  fn is_builtin_cwd(raw: i32) -> bool {
    raw == libc::AT_FDCWD
  }

  #[cfg(not(unix))]
  fn is_builtin_cwd(_raw: i32) -> bool {
    false
  }

  fn choose_len(&mut self, total: usize, may_be_zero: bool) -> usize {
    if total == 0 {
      return 0;
    }
    if may_be_zero
      && self.payload_rng.range_u64(self.scenario.short_io_zero_mod as u64) == 0
    {
      return 0;
    }
    1 + self.payload_rng.range_usize(total)
  }

  fn os_path(path: &std::ffi::OsString) -> Vec<u8> {
    #[cfg(unix)]
    {
      use std::os::unix::ffi::OsStrExt;
      Self::normalize_path(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
      Self::normalize_path(path.to_string_lossy().as_bytes())
    }
  }

  fn normalize_path_from(dir: &[u8], path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
      return dir.to_vec();
    }
    if path[0] == b'/' {
      return Self::normalize_path(path);
    }
    let mut out = dir.to_vec();
    if out.len() > 1 && !out.ends_with(b"/") {
      out.push(b'/');
    }
    out.extend_from_slice(path);
    while out.len() > 1 && out.ends_with(b"/") {
      out.pop();
    }
    out
  }

  fn normalize_path(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
      return b"/".to_vec();
    }
    let mut out = if bytes[0] == b'/' {
      bytes.to_vec()
    } else {
      let mut v = b"/sim/".to_vec();
      v.extend_from_slice(bytes);
      v
    };
    while out.len() > 1 && out.ends_with(b"/") {
      out.pop();
    }
    out
  }

  fn fake_cwd(&self) -> &[u8] {
    &self.scenario.cwd
  }

  fn resolve_dir_base(&self, dir_fd: &Resource) -> Result<Vec<u8>, isize> {
    let raw = Self::resource_key(dir_fd);
    if Self::is_builtin_cwd(raw) {
      return Ok(self.fake_cwd().to_vec());
    }
    let Some(SimResource::File(handle)) = self.world.resources.get(&raw) else {
      return Err(-(libc::EBADF as isize));
    };
    match self.world.fs.get(&handle.path) {
      Some(SimNode::Directory { .. }) => Ok(handle.path.clone()),
      Some(_) => Err(-(libc::ENOTDIR as isize)),
      None => Err(-(libc::ENOENT as isize)),
    }
  }

  fn resolve_open_path(
    &self,
    dir_fd: &Resource,
    path: &std::ffi::OsString,
  ) -> Result<Vec<u8>, isize> {
    #[cfg(unix)]
    let bytes = {
      use std::os::unix::ffi::OsStrExt;
      path.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let bytes = path.to_string_lossy().as_bytes();
    if bytes.first() == Some(&b'/') {
      return Ok(Self::normalize_path(bytes));
    }
    let base = self.resolve_dir_base(dir_fd)?;
    Ok(Self::normalize_path_from(&base, bytes))
  }

  fn write_c_string_bytes(
    buf: NonNull<u8>,
    buf_len: usize,
    bytes: &[u8],
  ) -> isize {
    if buf_len < bytes.len() + 1 {
      return -(libc::ERANGE as isize);
    }
    // SAFETY: caller provides a writable output buffer of `buf_len` bytes.
    let out = unsafe { std::slice::from_raw_parts_mut(buf.as_ptr(), buf_len) };
    out[..bytes.len()].copy_from_slice(bytes);
    out[bytes.len()] = 0;
    bytes.len() as isize
  }

  fn write_from_bytes(&mut self, ptr: *mut u8, src: &[u8]) {
    if src.is_empty() {
      return;
    }
    // SAFETY: caller provided a live writable buffer of at least `src.len()`.
    let out = unsafe { std::slice::from_raw_parts_mut(ptr, src.len()) };
    out.copy_from_slice(src);
  }

  fn extract_iovec_bytes(iovecs: NonNull<RawBuf>, iov_count: usize) -> Vec<u8> {
    if iov_count == 0 {
      return Vec::new();
    }
    // SAFETY: iovec metadata stays valid for the op lifetime.
    let iovecs =
      unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), iov_count) };
    let mut out = Vec::with_capacity(iovecs.iter().map(|iov| iov.len).sum());
    for iov in iovecs {
      if iov.len == 0 || iov.ptr.is_null() {
        continue;
      }
      // SAFETY: each `RawBuf` is valid for reads of `len` bytes during the op lifetime.
      let bytes = unsafe { std::slice::from_raw_parts(iov.ptr, iov.len) };
      out.extend_from_slice(bytes);
    }
    out
  }

  fn write_to_iovecs(
    iovecs: NonNull<RawBuf>,
    iov_count: usize,
    bytes: &[u8],
  ) -> usize {
    if iov_count == 0 || bytes.is_empty() {
      return 0;
    }
    // SAFETY: iovec metadata stays valid for the op lifetime.
    let iovecs =
      unsafe { std::slice::from_raw_parts_mut(iovecs.as_ptr(), iov_count) };
    let mut written = 0usize;
    for iov in iovecs {
      if written == bytes.len() {
        break;
      }
      if iov.ptr.is_null() {
        break;
      }
      let chunk = (bytes.len() - written).min(iov.len);
      // SAFETY: caller provided writable buffers described by `RawBuf`.
      let dst = unsafe { std::slice::from_raw_parts_mut(iov.ptr, chunk) };
      dst.copy_from_slice(&bytes[written..written + chunk]);
      written += chunk;
    }
    written
  }

  fn write_to_msg_recv(&mut self, msg: MsgRecv, bytes: &[u8]) -> usize {
    // SAFETY: op model keeps the descriptor array alive for the op lifetime.
    // We only read the `MsgBufMut` descriptors here; the actual pointed-to
    // payload buffers are mutated separately via `write_from_bytes`.
    let bufs = unsafe {
      std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
    };
    let mut written = 0usize;
    for buf in bufs {
      if written == bytes.len() {
        break;
      }
      let chunk = (bytes.len() - written).min(buf.len);
      self.write_from_bytes(buf.ptr.as_ptr(), &bytes[written..written + chunk]);
      written += chunk;
    }
    written
  }

  fn read_msg_send(msg: MsgSend) -> Vec<u8> {
    // SAFETY: op model keeps send buffers alive for the op lifetime.
    let bufs = unsafe {
      std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
    };
    let mut out = Vec::with_capacity(bufs.iter().map(|buf| buf.len).sum());
    for buf in bufs {
      // SAFETY: each `MsgBuf` is valid for reads during the op lifetime.
      let bytes =
        unsafe { std::slice::from_raw_parts(buf.ptr.as_ptr(), buf.len) };
      out.extend_from_slice(bytes);
    }
    out
  }

  fn socket_addr_key(addr: &SocketAddrBuf) -> Vec<u8> {
    match addr.family {
      SocketAddrFamily::Ipv4 => {
        let mut out = b"ipv4:".to_vec();
        out.extend_from_slice(&addr.ip[..4]);
        out.extend_from_slice(&addr.port_be.to_be_bytes());
        out
      }
      SocketAddrFamily::Ipv6 => {
        let mut out = b"ipv6:".to_vec();
        out.extend_from_slice(&addr.ip);
        out.extend_from_slice(&addr.port_be.to_be_bytes());
        out
      }
      SocketAddrFamily::Unix => {
        let mut out = b"unix:".to_vec();
        out.extend_from_slice(&addr.unix_path[..addr.unix_path_len as usize]);
        out
      }
      SocketAddrFamily::Unspecified => b"unspecified".to_vec(),
    }
  }

  fn file_stat_for_node(node: &SimNode) -> FileStat {
    match node {
      SimNode::Directory { mode } => FileStat {
        file_type: FileType::Directory,
        size: 0,
        permissions: mode & 0o7777,
        mode: (libc::S_IFDIR as u32) | mode,
        nlink: 1,
        uid: 0,
        gid: 0,
      },
      SimNode::File { bytes, mode } => FileStat {
        file_type: FileType::File,
        size: bytes.len() as u64,
        permissions: mode & 0o7777,
        mode: (libc::S_IFREG as u32) | mode,
        nlink: 1,
        uid: 0,
        gid: 0,
      },
      SimNode::Symlink { target, mode } => FileStat {
        file_type: FileType::Symlink,
        size: target.len() as u64,
        permissions: mode & 0o7777,
        mode: (libc::S_IFLNK as u32) | mode,
        nlink: 1,
        uid: 0,
        gid: 0,
      },
    }
  }

  fn resolve_path_node<'a>(
    &'a self,
    path: &[u8],
    follow_symlink: bool,
  ) -> Option<&'a SimNode> {
    let node = self.world.fs.get(path)?;
    if follow_symlink && let SimNode::Symlink { target, .. } = node {
      return self.world.fs.get(target);
    }
    Some(node)
  }

  fn create_socket_resource(
    &mut self,
    domain: SockDomain,
    ty: SockType,
    proto: SockProto,
  ) -> i32 {
    let fd = self.allocate_resource_id();
    self.world.resources.insert(
      fd,
      SimResource::Socket(SimSocketHandle {
        domain,
        ty,
        proto,
        inbox: VecDeque::new(),
        bound: None,
        peer_addr: None,
        listening: false,
        backlog: 0,
        shutdown_read: false,
        shutdown_write: false,
        pending_accept: VecDeque::new(),
        peer: None,
      }),
    );
    fd
  }

  fn make_file_handle(&self, path: Vec<u8>, flags: i32) -> SimFileHandle {
    let accmode = flags & libc::O_ACCMODE;
    let readable = accmode != libc::O_WRONLY;
    let writable = accmode != libc::O_RDONLY;
    SimFileHandle {
      path,
      cursor: 0,
      readable,
      writable,
      append: flags & libc::O_APPEND != 0,
    }
  }

  fn file_bytes_range(&self, path: &[u8], start: usize, len: usize) -> Vec<u8> {
    let Some(SimNode::File { bytes, .. }) = self.world.fs.get(path) else {
      return Vec::new();
    };
    if start >= bytes.len() {
      return Vec::new();
    }
    let end = start.saturating_add(len).min(bytes.len());
    bytes[start..end].to_vec()
  }

  fn try_read_socket_bytes(
    &mut self,
    raw: i32,
    len_hint: usize,
    allow_pending: bool,
  ) -> Result<PendingResult<Vec<u8>>, isize> {
    let Some(resource) = self.world.resources.remove(&raw) else {
      return Err(-(libc::EBADF as isize));
    };
    let SimResource::Socket(mut socket) = resource else {
      self.world.resources.insert(raw, resource);
      return Err(-(libc::EBADF as isize));
    };

    if socket.shutdown_read {
      self.world.resources.insert(raw, SimResource::Socket(socket));
      return Ok(PendingResult::Ready(Vec::new()));
    }
    if socket.inbox.is_empty() {
      let is_connected = socket.peer.is_some();
      self.world.resources.insert(raw, SimResource::Socket(socket));
      if allow_pending && is_connected {
        return Ok(PendingResult::Pending);
      }
      return Err(-(libc::ENOTSUP as isize));
    }

    let take = self.choose_len(socket.inbox.len().min(len_hint.max(1)), false);
    let mut out = Vec::with_capacity(take);
    for _ in 0..take {
      if let Some(byte) = socket.inbox.pop_front() {
        out.push(byte);
      }
    }
    self.world.resources.insert(raw, SimResource::Socket(socket));
    Ok(PendingResult::Ready(out))
  }

  fn poll_pending_ops(&mut self) {
    let mut idx = 0usize;
    while idx < self.world.pending.len() {
      let (id, op) = self.world.pending.remove(idx);
      match self.process_pending_op(id, op) {
        PendingAction::Complete(result) => {
          self.record_trace(format!(
            "tick={} deliver-pending id={} result={}",
            self.world.tick, id, result
          ));
          self.ready.push(OpCompleted::new(id, result));
        }
        PendingAction::KeepPending(op) => {
          self.world.pending.insert(idx, (id, op));
          idx += 1;
        }
      }
    }
  }

  fn process_pending_op(&mut self, _id: u64, op: Op) -> PendingAction {
    match op {
      Op::Read { fd, iovecs, iov_count, offset, flags } => {
        if offset < -1 || flags.bits() < 0 || offset >= 0 {
          return PendingAction::Complete(-(libc::EINVAL as isize));
        }
        let raw = Self::resource_key(&fd);
        let total: usize =
          unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), iov_count) }
            .iter()
            .map(|iov| iov.len)
            .sum();
        match self.try_read_socket_bytes(raw, total, true) {
          Ok(PendingResult::Ready(bytes)) => PendingAction::Complete(
            Self::write_to_iovecs(iovecs, iov_count, &bytes) as isize,
          ),
          Ok(PendingResult::Pending) => PendingAction::KeepPending(Op::Read {
            fd,
            iovecs,
            iov_count,
            offset,
            flags,
          }),
          Err(err) => PendingAction::Complete(err),
        }
      }
      Op::Recv { fd, msg, flags } => {
        if flags.bits() < 0 {
          return PendingAction::Complete(-(libc::EINVAL as isize));
        }
        let raw = Self::resource_key(&fd);
        let total = unsafe {
          std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
        }
        .iter()
        .map(|buf| buf.len)
        .sum();
        if let Some(out) = msg.from {
          let addr = self
            .world
            .resources
            .get(&raw)
            .and_then(|resource| match resource {
              SimResource::Socket(socket) => socket.peer_addr,
              SimResource::File(_) => None,
            })
            .unwrap_or_else(SocketAddrBuf::unspecified);
          unsafe {
            out.as_ptr().write(addr);
          }
        }
        match self.try_read_socket_bytes(raw, total, true) {
          Ok(PendingResult::Ready(bytes)) => {
            PendingAction::Complete(self.write_to_msg_recv(msg, &bytes) as isize)
          }
          Ok(PendingResult::Pending) => {
            PendingAction::KeepPending(Op::Recv { fd, msg, flags })
          }
          Err(err) => PendingAction::Complete(err),
        }
      }
      Op::Accept { fd, addr } => {
        let raw = Self::resource_key(&fd);
        let Some(resource) = self.world.resources.remove(&raw) else {
          return PendingAction::Complete(-(libc::EBADF as isize));
        };
        let SimResource::Socket(mut listener) = resource else {
          self.world.resources.insert(raw, resource);
          return PendingAction::Complete(-(libc::EBADF as isize));
        };
        if !listener.listening {
          self.world.resources.insert(raw, SimResource::Socket(listener));
          return PendingAction::Complete(-(libc::EINVAL as isize));
        }
        let Some(accepted_fd) = listener.pending_accept.pop_front() else {
          self.world.resources.insert(raw, SimResource::Socket(listener));
          return PendingAction::KeepPending(Op::Accept { fd, addr });
        };
        let peer_addr = self
          .world
          .resources
          .get(&accepted_fd)
          .and_then(|resource| match resource {
            SimResource::Socket(socket) => socket.peer_addr,
            SimResource::File(_) => None,
          })
          .unwrap_or_else(SocketAddrBuf::unspecified);
        unsafe {
          addr.as_ptr().write(peer_addr);
        }
        self.world.resources.insert(raw, SimResource::Socket(listener));
        PendingAction::Complete(accepted_fd as isize)
      }
      _ => PendingAction::Complete(-(libc::EINVAL as isize)),
    }
  }

  fn read_bytes_from_resource(
    &mut self,
    fd: &Resource,
    len_hint: usize,
    offset: Option<usize>,
  ) -> Result<Vec<u8>, isize> {
    let raw = Self::resource_key(fd);
    if offset.is_some() && Self::is_builtin_stdin(raw) {
      return Err(-(libc::ESPIPE as isize));
    }
    if Self::is_builtin_stdin(raw) {
      return Err(-(libc::ENOTSUP as isize));
    }
    let Some(resource) = self.world.resources.remove(&raw) else {
      return Err(-(libc::EBADF as isize));
    };
    match resource {
      SimResource::File(mut handle) => {
        if !handle.readable {
          self.world.resources.insert(raw, SimResource::File(handle));
          return Err(-(libc::EBADF as isize));
        }
        let start = offset.unwrap_or(handle.cursor);
        let remaining = match self.world.fs.get(&handle.path) {
          Some(SimNode::File { bytes, .. }) => {
            bytes.len().saturating_sub(start)
          }
          _ => 0,
        };
        let available = if remaining == 0 {
          0
        } else {
          self.choose_len(remaining.min(len_hint.max(1)), false)
        };
        if offset.is_none() {
          handle.cursor += available;
        }
        let bytes = self.file_bytes_range(&handle.path, start, available);
        self.world.resources.insert(raw, SimResource::File(handle));
        Ok(bytes)
      }
      SimResource::Socket(socket) => {
        if offset.is_some() {
          self.world.resources.insert(raw, SimResource::Socket(socket));
          return Err(-(libc::ESPIPE as isize));
        }
        self.world.resources.insert(raw, SimResource::Socket(socket));
        match self.try_read_socket_bytes(raw, len_hint, false)? {
          PendingResult::Ready(bytes) => Ok(bytes),
          PendingResult::Pending => Err(-(libc::ENOTSUP as isize)),
        }
      }
    }
  }

  fn write_bytes_to_resource(
    &mut self,
    fd: &Resource,
    bytes: &[u8],
    offset: Option<usize>,
  ) -> Result<usize, isize> {
    let raw = Self::resource_key(fd);
    if offset.is_some() && Self::is_builtin_stdout(raw) {
      return Err(-(libc::ESPIPE as isize));
    }
    if Self::is_builtin_stdout(raw) {
      return Ok(self.choose_len(bytes.len().max(1), false));
    }
    let Some(resource) = self.world.resources.remove(&raw) else {
      return Err(-(libc::EBADF as isize));
    };
    match resource {
      SimResource::File(mut handle) => {
        if !handle.writable {
          self.world.resources.insert(raw, SimResource::File(handle));
          return Err(-(libc::EBADF as isize));
        }
        let to_write =
          self.choose_len(bytes.len().max(1), false).min(bytes.len());
        let Some(SimNode::File { bytes: file_bytes, .. }) =
          self.world.fs.get_mut(&handle.path)
        else {
          self.world.resources.insert(raw, SimResource::File(handle));
          return Err(-(libc::ENOENT as isize));
        };
        let start = if handle.append {
          file_bytes.len()
        } else {
          offset.unwrap_or(handle.cursor)
        };
        let needed = start + to_write;
        if file_bytes.len() < needed {
          file_bytes.resize(needed, 0);
        }
        file_bytes[start..start + to_write].copy_from_slice(&bytes[..to_write]);
        if offset.is_none() || handle.append {
          handle.cursor = start + to_write;
        }
        self.world.resources.insert(raw, SimResource::File(handle));
        Ok(to_write)
      }
      SimResource::Socket(socket) => {
        if offset.is_some() {
          self.world.resources.insert(raw, SimResource::Socket(socket));
          return Err(-(libc::ESPIPE as isize));
        }
        if socket.shutdown_write {
          self.world.resources.insert(raw, SimResource::Socket(socket));
          return Err(-(libc::EPIPE as isize));
        }
        let to_write =
          self.choose_len(bytes.len().max(1), false).min(bytes.len());
        if let Some(peer) = socket.peer {
          let ready_at_tick = self.world.tick + self.event_delay_ticks();
          let payload = bytes[..to_write].to_vec();
          self.world.events.push(ScheduledEvent::StreamDelivered {
            ready_at_tick,
            to_fd: peer,
            bytes: payload,
          });
          self.record_trace(format!(
            "tick={} schedule-stream-delivery from_fd={} to_fd={} len={} at={}",
            self.world.tick, raw, peer, to_write, ready_at_tick
          ));
        }
        self.world.resources.insert(raw, SimResource::Socket(socket));
        Ok(to_write)
      }
    }
  }

  fn complete_op(&mut self, op: Op) -> OpAction {
    if let Some(fault) = self.maybe_inject_fault(&op) {
      return OpAction::Complete(fault);
    }

    match op {
      Op::Read { fd, iovecs, iov_count, offset, flags } => {
        if offset < -1 {
          return OpAction::Complete(-(libc::EINVAL as isize));
        }
        if flags.bits() < 0 {
          return OpAction::Complete(-(libc::ENOTSUP as isize));
        }
        let total: usize =
          // SAFETY: read-only access to caller-provided iovec metadata.
          unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), iov_count) }
            .iter()
            .map(|iov| iov.len)
            .sum();
        if offset >= 0 {
          return match self.read_bytes_from_resource(
            &fd,
            total,
            Some(offset as usize),
          ) {
            Ok(bytes) => OpAction::Complete(Self::write_to_iovecs(
              iovecs, iov_count, &bytes,
            ) as isize),
            Err(err) => OpAction::Complete(err),
          };
        }
        let raw = Self::resource_key(&fd);
        if let Some(SimResource::Socket(_)) = self.world.resources.get(&raw) {
          return match self.try_read_socket_bytes(raw, total, true) {
            Ok(PendingResult::Ready(bytes)) => OpAction::Complete(
              Self::write_to_iovecs(iovecs, iov_count, &bytes) as isize,
            ),
            Ok(PendingResult::Pending) => OpAction::Pending(Op::Read {
              fd,
              iovecs,
              iov_count,
              offset,
              flags,
            }),
            Err(err) => OpAction::Complete(err),
          };
        }
        match self.read_bytes_from_resource(&fd, total, None) {
          Ok(bytes) => OpAction::Complete(Self::write_to_iovecs(
            iovecs, iov_count, &bytes,
          ) as isize),
          Err(err) => OpAction::Complete(err),
        }
      }
      Op::Write { fd, iovecs, iov_count, offset, flags } => {
        if offset < -1 {
          return OpAction::Complete(-(libc::EINVAL as isize));
        }
        if flags.bits() < 0 {
          return OpAction::Complete(-(libc::ENOTSUP as isize));
        }
        let bytes = Self::extract_iovec_bytes(iovecs, iov_count);
        match self.write_bytes_to_resource(
          &fd,
          &bytes,
          (offset >= 0).then_some(offset as usize),
        ) {
          Ok(len) => OpAction::Complete(len as isize),
          Err(err) => OpAction::Complete(err),
        }
      }
      Op::Recv { fd, msg, flags } => {
        if flags.bits() < 0 {
          return OpAction::Complete(-(libc::EINVAL as isize));
        }
        let total =
          // SAFETY: `MsgRecv` buffers stay alive for the op lifetime.
          unsafe { std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get()) }
            .iter()
            .map(|buf| buf.len)
            .sum();
        let raw = Self::resource_key(&fd);
        if let Some(out) = msg.from {
          let addr = self
            .world
            .resources
            .get(&raw)
            .and_then(|resource| match resource {
              SimResource::Socket(socket) => socket.peer_addr,
              SimResource::File(_) => None,
            })
            .unwrap_or_else(SocketAddrBuf::unspecified);
          unsafe {
            out.as_ptr().write(addr);
          }
        }
        match self.try_read_socket_bytes(raw, total, true) {
          Ok(PendingResult::Ready(bytes)) => {
            OpAction::Complete(self.write_to_msg_recv(msg, &bytes) as isize)
          }
          Ok(PendingResult::Pending) => {
            OpAction::Pending(Op::Recv { fd, msg, flags })
          }
          Err(_) => match self.read_bytes_from_resource(&fd, total, None) {
            Ok(bytes) => {
              OpAction::Complete(self.write_to_msg_recv(msg, &bytes) as isize)
            }
            Err(err) => OpAction::Complete(err),
          },
        }
      }
      Op::Send { fd, msg, flags } => {
        if flags.bits() < 0 {
          return OpAction::Complete(-(libc::EINVAL as isize));
        }
        let bytes = Self::read_msg_send(msg);
        match self.write_bytes_to_resource(&fd, &bytes, None) {
          Ok(len) => OpAction::Complete(len as isize),
          Err(err) => OpAction::Complete(err),
        }
      }
      Op::Accept { fd, addr } => {
        match self.process_pending_op(0, Op::Accept { fd, addr }) {
          PendingAction::Complete(result) => OpAction::Complete(result),
          PendingAction::KeepPending(op) => OpAction::Pending(op),
        }
      }
      Op::Connect { fd, addr } => {
        let raw = Self::resource_key(&fd);
        let key = Self::socket_addr_key(&addr);
        let Some(SimResource::Socket(mut socket)) =
          self.world.resources.remove(&raw)
        else {
          return OpAction::Complete(-(libc::EBADF as isize));
        };
        let Some(listener_fd) = self.world.bound_listeners.get(&key).copied()
        else {
          self.world.resources.insert(raw, SimResource::Socket(socket));
          return OpAction::Complete(-(libc::ENOENT as isize));
        };
        let Some(SimResource::Socket(listener_socket)) =
          self.world.resources.get(&listener_fd)
        else {
          self.world.resources.insert(raw, SimResource::Socket(socket));
          return OpAction::Complete(-(libc::ENOENT as isize));
        };
        if !listener_socket.listening {
          self.world.resources.insert(raw, SimResource::Socket(socket));
          return OpAction::Complete(-(libc::EINVAL as isize));
        }
        let accepted_fd =
          self.create_socket_resource(socket.domain, socket.ty, socket.proto);
        socket.peer = Some(accepted_fd);
        socket.peer_addr = Some(addr);
        if let Some(SimResource::Socket(accepted)) =
          self.world.resources.get_mut(&accepted_fd)
        {
          accepted.peer = Some(raw);
          accepted.peer_addr = socket.bound;
        }
        self.world.resources.insert(raw, SimResource::Socket(socket));
        let ready_at_tick = self.world.tick + self.event_delay_ticks();
        OpAction::StartConnect { accepted_fd, listener_fd, ready_at_tick }
      }
      Op::OpenAt { dir_fd, path, flags, .. } => {
        let path = match self.resolve_open_path(&dir_fd, &path) {
          Ok(path) => path,
          Err(err) => return OpAction::Complete(err),
        };
        let exists = self.world.fs.contains_key(&path);
        if exists
          && flags.bits() & libc::O_CREAT != 0
          && flags.bits() & libc::O_EXCL != 0
        {
          return OpAction::Complete(-(libc::EEXIST as isize));
        }
        if !exists {
          if flags.bits() & libc::O_CREAT == 0 {
            return OpAction::Complete(-(libc::ENOENT as isize));
          }
          self.world.fs.insert(
            path.clone(),
            SimNode::File { bytes: Vec::new(), mode: 0o644 },
          );
        }
        if flags.bits() & libc::O_DIRECTORY != 0 {
          let Some(SimNode::Directory { .. }) = self.world.fs.get(&path) else {
            return OpAction::Complete(-(libc::ENOTDIR as isize));
          };
        }
        if flags.bits() & libc::O_TRUNC != 0
          && let Some(SimNode::File { bytes, .. }) =
            self.world.fs.get_mut(&path)
        {
          bytes.clear();
        }
        let fd = self.allocate_resource_id();
        let handle = self.make_file_handle(path, flags.bits());
        self.world.resources.insert(fd, SimResource::File(handle));
        OpAction::Complete(fd as isize)
      }
      Op::UnlinkAt { path, kind, .. } => {
        let path = Self::os_path(&path);
        let Some(node) = self.world.fs.get(&path) else {
          return OpAction::Complete(-(libc::ENOENT as isize));
        };
        if matches!(kind, UnlinkKind::Directory)
          && !matches!(node, SimNode::Directory { .. })
        {
          return OpAction::Complete(-(libc::ENOENT as isize));
        }
        self.world.fs.remove(&path);
        OpAction::Complete(0)
      }
      Op::RenameAt { old_path, new_path, .. } => {
        let old_path = Self::os_path(&old_path);
        let new_path = Self::os_path(&new_path);
        let Some(node) = self.world.fs.remove(&old_path) else {
          return OpAction::Complete(-(libc::ENOENT as isize));
        };
        self.world.fs.insert(new_path, node);
        OpAction::Complete(0)
      }
      Op::MkdirAt { path, .. } => {
        let path = Self::os_path(&path);
        if self.world.fs.contains_key(&path) {
          return OpAction::Complete(-(libc::EEXIST as isize));
        }
        self.world.fs.insert(path, SimNode::Directory { mode: 0o755 });
        OpAction::Complete(0)
      }
      Op::LinkAt { source_path, new_path, kind, .. } => {
        let source_path = Self::os_path(&source_path);
        let new_path = Self::os_path(&new_path);
        match kind {
          op::LinkKind::Hard => {
            let Some(node) = self.world.fs.get(&source_path).cloned() else {
              return OpAction::Complete(-(libc::ENOENT as isize));
            };
            self.world.fs.insert(new_path, node);
          }
          op::LinkKind::Soft => {
            self.world.fs.insert(
              new_path,
              SimNode::Symlink { target: source_path, mode: 0o777 },
            );
          }
        }
        OpAction::Complete(0)
      }
      Op::ReadlinkAt { path, buf, buf_len, .. } => {
        let path = Self::os_path(&path);
        let Some(SimNode::Symlink { target, .. }) = self.world.fs.get(&path)
        else {
          return OpAction::Complete(-(libc::ENOENT as isize));
        };
        OpAction::Complete(Self::write_c_string_bytes(buf, buf_len, target))
      }
      Op::GetCwd { out } => {
        let payload = self.fake_cwd();
        #[cfg(unix)]
        let cwd = {
          use std::os::unix::ffi::OsStringExt;
          std::ffi::OsString::from_vec(payload.to_vec())
        };
        #[cfg(not(unix))]
        let cwd =
          std::ffi::OsString::from(String::from_utf8_lossy(payload).as_ref());
        unsafe { out.as_ptr().write(cwd) };
        OpAction::Complete(0)
      }
      Op::Spawn { spec } => {
        #[cfg(unix)]
        let path = {
          use std::os::unix::ffi::OsStrExt;
          Self::normalize_path(spec.program.as_bytes())
        };
        #[cfg(not(unix))]
        let path =
          Self::normalize_path(spec.program.to_string_lossy().as_bytes());

        if path.is_empty() || !self.world.fs.contains_key(&path) {
          return OpAction::Complete(-(libc::ENOENT as isize));
        }
        OpAction::Complete(self.allocate_pid() as isize)
      }
      Op::Socket { domain, ty, proto } => {
        if op::socket_to_raw(domain, ty, proto).is_err() {
          return OpAction::Complete(-(libc::EINVAL as isize));
        }
        OpAction::Complete(
          self.create_socket_resource(domain, ty, proto) as isize
        )
      }
      Op::Bind { fd, addr } => {
        let raw = Self::resource_key(&fd);
        let addr = op::socket_addr_into_buf(addr);
        let key = Self::socket_addr_key(&addr);
        if self.world.bound_listeners.contains_key(&key)
          || self
            .world
            .occupied_addr_keys
            .iter()
            .any(|occupied| occupied == &key)
        {
          return OpAction::Complete(-(libc::EADDRINUSE as isize));
        }
        let Some(SimResource::Socket(mut socket)) =
          self.world.resources.remove(&raw)
        else {
          if Self::is_builtin_stdin(raw)
            || Self::is_builtin_stdout(raw)
            || Self::is_builtin_cwd(raw)
          {
            return OpAction::Complete(-(libc::ENOTSOCK as isize));
          }
          return OpAction::Complete(-(libc::EBADF as isize));
        };
        socket.bound = Some(addr);
        self.world.resources.insert(raw, SimResource::Socket(socket));
        self.world.bound_listeners.insert(key, raw);
        OpAction::Complete(0)
      }
      Op::Listen { fd, backlog } => {
        let raw = Self::resource_key(&fd);
        let Some(SimResource::Socket(mut socket)) =
          self.world.resources.remove(&raw)
        else {
          if Self::is_builtin_stdin(raw)
            || Self::is_builtin_stdout(raw)
            || Self::is_builtin_cwd(raw)
          {
            return OpAction::Complete(-(libc::ENOTSOCK as isize));
          }
          return OpAction::Complete(-(libc::EBADF as isize));
        };
        socket.listening = true;
        socket.backlog = backlog;
        self.world.resources.insert(raw, SimResource::Socket(socket));
        OpAction::Complete(0)
      }
      Op::Shutdown { fd, how } => {
        let raw = Self::resource_key(&fd);
        let Some(SimResource::Socket(mut socket)) =
          self.world.resources.remove(&raw)
        else {
          if Self::is_builtin_stdin(raw)
            || Self::is_builtin_stdout(raw)
            || Self::is_builtin_cwd(raw)
          {
            return OpAction::Complete(-(libc::ENOTSOCK as isize));
          }
          return OpAction::Complete(-(libc::EBADF as isize));
        };
        let peer = socket.peer;
        match how {
          ShutdownHow::Read => socket.shutdown_read = true,
          ShutdownHow::Write => socket.shutdown_write = true,
          ShutdownHow::Both => {
            socket.shutdown_read = true;
            socket.shutdown_write = true;
          }
        }
        self.world.resources.insert(raw, SimResource::Socket(socket));
        if matches!(how, ShutdownHow::Write | ShutdownHow::Both)
          && let Some(peer_fd) = peer
        {
          let ready_at_tick = self.world.tick + self.event_delay_ticks();
          self.world.events.push(ScheduledEvent::SocketShutdownRead {
            ready_at_tick,
            fd: peer_fd,
          });
          self.record_trace(format!(
            "tick={} schedule-peer-shutdown-read from_fd={} to_fd={} at={}",
            self.world.tick, raw, peer_fd, ready_at_tick
          ));
        }
        OpAction::Complete(0)
      }
      Op::Fsync { fd } => {
        let raw = Self::resource_key(&fd);
        match self.world.resources.get(&raw) {
          Some(SimResource::File(_)) => OpAction::Complete(0),
          Some(SimResource::Socket(_)) => {
            OpAction::Complete(-(libc::EINVAL as isize))
          }
          None if Self::is_builtin_cwd(raw) => {
            OpAction::Complete(-(libc::EBADF as isize))
          }
          None
            if Self::is_builtin_stdin(raw) || Self::is_builtin_stdout(raw) =>
          {
            OpAction::Complete(-(libc::EINVAL as isize))
          }
          None => OpAction::Complete(-(libc::EBADF as isize)),
        }
      }
      Op::Stat { target, out } => {
        let stat = match target {
          op::StatTarget::Path { path, follow_symlinks, .. } => {
            let path = Self::os_path(&path);
            let Some(node) = self.resolve_path_node(&path, follow_symlinks)
            else {
              return OpAction::Complete(-(libc::ENOENT as isize));
            };
            Self::file_stat_for_node(node)
          }
          op::StatTarget::Fd { fd } => {
            let raw = Self::resource_key(&fd);
            match self.world.resources.get(&raw) {
              Some(SimResource::File(handle)) => {
                let Some(node) = self.world.fs.get(&handle.path) else {
                  return OpAction::Complete(-(libc::EBADF as isize));
                };
                Self::file_stat_for_node(node)
              }
              Some(SimResource::Socket(_)) => FileStat {
                file_type: crate::backend::op::FileType::Socket,
                size: 0,
                permissions: 0,
                mode: libc::S_IFSOCK as u32,
                nlink: 1,
                uid: 0,
                gid: 0,
              },
              None => return OpAction::Complete(-(libc::EBADF as isize)),
            }
          }
        };
        // SAFETY: `out` points to caller-owned output storage.
        unsafe {
          out.as_ptr().write(stat);
        }
        OpAction::Complete(0)
      }
      Op::ReadDir {
        raw_buf,
        raw_cap,
        entries,
        entries_cap,
        opaque,
        opaque_drop,
        out,
        ..
      } => {
        let dir = self.scenario.readdir_root.as_slice();
        let mut names: Vec<Vec<u8>> = self
          .world
          .fs
          .keys()
          .filter_map(|path| {
            if path == dir {
              return None;
            }
            let mut parent = path.clone();
            if let Some(pos) = parent.iter().rposition(|&b| b == b'/') {
              parent.truncate(pos.max(1));
              if pos == 0 {
                parent = b"/".to_vec();
              }
            }
            if parent == dir {
              path.split(|&b| b == b'/').next_back().map(|name| name.to_vec())
            } else {
              None
            }
          })
          .collect();
        self.topology_rng.shuffle(&mut names);
        let targets = ReadDirTargets {
          raw_buf,
          raw_cap,
          entries,
          entries_cap,
          out,
          opaque,
          opaque_drop,
        };
        self.write_readdir(targets, &names);
        OpAction::Complete(0)
      }
      Op::Nop => OpAction::Complete(0),
    }
  }

  fn write_readdir(&mut self, targets: ReadDirTargets, names: &[Vec<u8>]) {
    // SAFETY: caller provides writable output storage for the duration of the op.
    let raw = unsafe {
      std::slice::from_raw_parts_mut(targets.raw_buf.as_ptr(), targets.raw_cap)
    };
    // SAFETY: caller provides `entries_cap` writable entry slots.
    let entries = unsafe {
      std::slice::from_raw_parts_mut(
        targets.entries.as_ptr(),
        targets.entries_cap,
      )
    };
    let mut raw_written = 0usize;
    let mut count = 0usize;
    for name in names.iter().take(targets.entries_cap) {
      if raw_written + name.len() > targets.raw_cap {
        break;
      }
      raw[raw_written..raw_written + name.len()].copy_from_slice(name);
      entries[count] = DirEntryRef {
        name_offset: raw_written as u32,
        name_len: name.len() as u16,
        file_type: Some(FileType::File),
        ino: Some(self.payload_rng.next_u64()),
      };
      raw_written += name.len();
      count += 1;
    }
    // SAFETY: output fields belong to the caller-owned `ReadDirBuf`.
    unsafe {
      targets.out.as_ptr().write(ReadDirResult {
        entries: count,
        raw_written,
        eof: true,
      });
      targets.opaque.as_ptr().write(std::ptr::null_mut());
      targets.opaque_drop.as_ptr().write(None);
    }
  }
}

impl IoBackend for DSBackend {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.cap = cap;
    self.initialized = true;
    self.queued = Vec::with_capacity(cap);
    self.world.events = Vec::with_capacity(cap);
    self.ready = Vec::with_capacity(cap.min(256));
    self.reset_world();
    Ok(())
  }

  fn push(&mut self, id: u64, op: Op, _step_bump: &mut Bump) {
    self.assert_initialized();
    self.assert_capacity();
    self.queued.push((id, op));
  }

  fn flush(&mut self) -> io::Result<()> {
    self.assert_initialized();
    let queued = mem::take(&mut self.queued);
    for (id, op) in queued {
      match self.complete_op(op) {
        OpAction::Complete(result) => self.schedule_completion(id, result),
        OpAction::Pending(op) => {
          self.record_trace(format!(
            "tick={} pending id={}",
            self.world.tick, id
          ));
          self.world.pending.push((id, op));
        }
        OpAction::StartConnect {
          accepted_fd,
          listener_fd,
          ready_at_tick,
          ..
        } => {
          self.record_trace(format!(
            "tick={} schedule-connect id={} listener_fd={} accepted_fd={} at={}",
            self.world.tick, id, listener_fd, accepted_fd, ready_at_tick
          ));
          self.world.events.push(ScheduledEvent::ConnectEstablished {
            ready_at_tick,
            id,
            listener_fd,
            accepted_fd,
          });
        }
      }
    }
    self.schedule_rng.shuffle(&mut self.ready);
    Ok(())
  }

  fn wait(
    &mut self,
    timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    self.assert_initialized();
    completed.clear();

    match timeout {
      Some(duration) if duration.is_zero() => {
        if self.ready.is_empty() && !self.world.events.is_empty() {
          self.advance_tick();
        }
      }
      Some(_) => {
        if self.ready.is_empty() {
          self.advance_tick();
        }
      }
      None => {
        while self.ready.is_empty() && !self.world.events.is_empty() {
          self.advance_tick();
        }
      }
    }

    completed.append(&mut self.ready);
    Ok(())
  }
}

impl DST {
  pub fn new() -> Self {
    Self::with_config(DSConfig::default())
  }

  pub fn with_config(config: DSConfig) -> Self {
    Self {
      config,
      inner: Rc::new(RefCell::new(DSTInner::new(config))),
      runners: Vec::new(),
      runner_index_by_node: BTreeMap::new(),
      ready_runners: VecDeque::new(),
    }
  }

  pub fn add_node<F>(&mut self, cap: usize, pump: F) -> io::Result<DSTNode>
  where
    F: FnMut(Lio) -> io::Result<()> + 'static,
  {
    let backend = create_dst_backend(self);
    let node = DSTNode { inner: self.inner.clone(), node_id: backend.node_id };
    let lio = Lio::new_with_backend(backend, cap)?;
    lio.pause_time();
    let runner_idx = self.runners.len();
    self.runners.push(DSTNodeRunner {
      lio,
      queued_for_pump: false,
      pump: Box::new(pump),
    });
    self.runner_index_by_node.insert(node.node_id, runner_idx);
    self.mark_runner_index_ready(runner_idx);
    Ok(node)
  }

  pub fn current_tick(&self) -> u64 {
    self.inner.borrow().tick
  }

  fn step(&mut self) -> bool {
    let mut progressed = false;
    let mut arbitration_round = 0_u64;
    loop {
      let Some((ready_at_tick, mut due_events)) =
        self.inner.borrow_mut().take_due_event_bucket()
      else {
        break;
      };

      while !due_events.is_empty() {
        let event = {
          let inner = self.inner.borrow();
          due_events
            .pop_next_event(&inner, ready_at_tick, arbitration_round)
            .expect("due event should exist")
        };

        progressed = true;
        arbitration_round = arbitration_round.saturating_add(1);
        match event {
          DSTEvent::ConnectEstablished {
            connect_id,
            client,
            listener,
            server_fd,
            server_peer_addr,
            ..
          } => {
            let link_available = self
              .inner
              .borrow_mut()
              .link_is_available(client.node_id, listener.node_id);
            let client_node = self.inner.borrow().node(client.node_id);
            let listener_node = self.inner.borrow().node(listener.node_id);
            let listener_bound = listener_node
              .borrow()
              .sockets
              .get(&listener.fd)
              .and_then(|socket| socket.bound);

            if !link_available {
              {
                let mut client_node = client_node.borrow_mut();
                if let Some(client_socket) =
                  client_node.sockets.get_mut(&client.fd)
                {
                  client_socket.state = DSTSocketState::Created;
                  client_socket.peer = None;
                }
                client_node.ready.push(OpCompleted::new(
                  connect_id,
                  -(libc::EHOSTUNREACH as isize),
                ));
              }

              {
                let mut listener_node = listener_node.borrow_mut();
                if let Some(listener_socket) =
                  listener_node.sockets.get_mut(&listener.fd)
                {
                  listener_socket.backlog_reserved =
                    listener_socket.backlog_reserved.saturating_sub(1);
                }
                listener_node.sockets.remove(&server_fd);
              }

              self.poll_pending_node(client.node_id);
              self.poll_pending_node(listener.node_id);
              continue;
            }

            {
              let mut client_node = client_node.borrow_mut();
              if let Some(client_socket) =
                client_node.sockets.get_mut(&client.fd)
              {
                client_socket.state = DSTSocketState::Connected;
                client_socket.peer = Some(DSTSocketRef {
                  node_id: listener.node_id,
                  fd: server_fd,
                });
                client_socket.peer_addr =
                  client_socket.peer_addr.or(listener_bound);
              }
              client_node.ready.push(OpCompleted::new(connect_id, 0));
            }

            {
              let mut listener_node = listener_node.borrow_mut();
              if let Some(accepted) = listener_node.sockets.get_mut(&server_fd)
              {
                accepted.state = DSTSocketState::Connected;
                accepted.peer = Some(client);
                accepted.peer_addr = Some(server_peer_addr);
              }
              if let Some(listener_socket) =
                listener_node.sockets.get_mut(&listener.fd)
              {
                listener_socket.backlog_reserved =
                  listener_socket.backlog_reserved.saturating_sub(1);
                listener_socket.pending_accept.push_back(server_fd);
              }
            }

            self.poll_pending_node(client.node_id);
            self.poll_pending_node(listener.node_id);
          }
          DSTEvent::StreamDelivered { from, to, bytes, .. } => {
            let link_available = self
              .inner
              .borrow_mut()
              .link_is_available(from.node_id, to.node_id);
            if !link_available {
              let mut backend = DSTBackend {
                local: None,
                node_id: from.node_id,
                node: self.inner.borrow().node(from.node_id),
                inner: self.inner.clone(),
                queued: Vec::new(),
              };
              backend.schedule_stream_link_reset(from, to, libc::ECONNRESET);
              continue;
            }
            {
              let node = self.inner.borrow().node(to.node_id);
              let mut node = node.borrow_mut();
              if let Some(socket) = node.sockets.get_mut(&to.fd) {
                if let DSTRecvQueue::Stream(inbox) = &mut socket.recv_queue {
                  inbox.extend(bytes);
                }
              }
            }
            self.poll_pending_node(to.node_id);
          }
          DSTEvent::DatagramDelivered {
            from, from_addr, to, packet, ..
          } => {
            if !self
              .inner
              .borrow_mut()
              .link_is_available(from.node_id, to.node_id)
            {
              continue;
            }
            {
              let node = self.inner.borrow().node(to.node_id);
              let mut node = node.borrow_mut();
              if let Some(socket) = node.sockets.get_mut(&to.fd) {
                if let DSTRecvQueue::Datagram(queue) = &mut socket.recv_queue {
                  socket.peer_addr = Some(from_addr);
                  queue.push_back(packet);
                }
              }
            }
            self.poll_pending_node(to.node_id);
          }
          DSTEvent::SocketShutdownRead { to, .. } => {
            {
              let node = self.inner.borrow().node(to.node_id);
              let mut node = node.borrow_mut();
              if let Some(socket) = node.sockets.get_mut(&to.fd) {
                socket.peer_write_closed = true;
                if socket.local_shutdown_write {
                  socket.state = DSTSocketState::Closed;
                }
              }
            }
            self.poll_pending_node(to.node_id);
          }
          DSTEvent::SocketReset { to, errno, .. } => {
            {
              let node = self.inner.borrow().node(to.node_id);
              let mut node = node.borrow_mut();
              if let Some(socket) = node.sockets.get_mut(&to.fd) {
                socket.peer_reset_error = Some(errno);
                socket.peer_write_closed = false;
                socket.state = DSTSocketState::Closed;
                socket.peer = None;
              }
            }
            self.poll_pending_node(to.node_id);
          }
        }
      }
    }

    while let Some((node_id, id, op)) =
      self.next_outgoing_network_op(arbitration_round)
    {
      progressed = true;
      arbitration_round = arbitration_round.saturating_add(1);
      self.execute_network_op(node_id, id, op);
    }
    progressed
  }

  fn next_outgoing_network_op(
    &mut self,
    arbitration_round: u64,
  ) -> Option<(DSTNodeId, u64, Op)> {
    let tick = self.inner.borrow().tick;
    let (chosen_node, stale_nodes) = {
      let inner = self.inner.borrow();
      let mut chosen = None;
      let mut chosen_key = None;
      let mut stale = Vec::new();
      for &node_id in &inner.active_outgoing_nodes {
        let Some(node) = inner.nodes.get(node_id.0 as usize) else {
          stale.push(node_id);
          continue;
        };
        if node.borrow().outgoing.is_empty() {
          stale.push(node_id);
          continue;
        }
        let rank = inner.delivery_rank(tick, arbitration_round, node_id);
        let key = (rank, node_id);
        if chosen_key.map_or(true, |best| key < best) {
          chosen_key = Some(key);
          chosen = Some(node_id);
        }
      }
      (chosen, stale)
    };
    if !stale_nodes.is_empty() {
      let mut inner = self.inner.borrow_mut();
      for node_id in stale_nodes {
        inner.active_outgoing_nodes.remove(&node_id);
      }
    }
    let chosen_node = chosen_node?;

    let node = self.inner.borrow().node(chosen_node);
    let mut node = node.borrow_mut();
    let (id, op) = node
      .outgoing
      .pop_front()
      .expect("chosen node should have queued outgoing network op");
    let became_empty = node.outgoing.is_empty();
    drop(node);
    if became_empty {
      self.inner.borrow_mut().active_outgoing_nodes.remove(&chosen_node);
    }
    Some((chosen_node, id, op))
  }

  fn execute_network_op(&mut self, node_id: DSTNodeId, id: u64, op: Op) {
    let node = self.inner.borrow().node(node_id);
    let mut backend = DSTBackend {
      local: None,
      node_id,
      node,
      inner: self.inner.clone(),
      queued: Vec::new(),
    };
    backend.network_complete_op(id, op);
    self.mark_runner_ready(node_id);
  }

  fn run_ready(&mut self) -> io::Result<bool> {
    let mut progressed_any = false;
    loop {
      let mut round_progress = self.step();
      while let Some(runner_idx) = self.ready_runners.pop_front() {
        let runner = &mut self.runners[runner_idx];
        runner.queued_for_pump = false;
        let guard = install_global(runner.lio.clone());
        let result = (runner.pump)(runner.lio.clone());
        drop(guard);
        result?;
        round_progress = true;
      }
      round_progress |= self.step();
      if !round_progress {
        break;
      }
      progressed_any = true;
    }
    Ok(progressed_any)
  }

  pub fn tick(&mut self) -> io::Result<bool> {
    let mut progressed = self.run_ready()?;
    {
      let mut inner = self.inner.borrow_mut();
      inner.tick = inner.tick.saturating_add(1);
    }
    for runner in &mut self.runners {
      runner.lio.advance_time_by_ticks(1);
    }
    for runner_idx in 0..self.runners.len() {
      self.mark_runner_index_ready(runner_idx);
    }
    progressed |= self.run_ready()?;
    Ok(progressed)
  }

  fn poll_pending_node(&mut self, node_id: DSTNodeId) {
    let node = self.inner.borrow().node(node_id);
    DSTBackend::poll_pending_node_state(node);
    self.mark_runner_ready(node_id);
  }

  fn mark_runner_ready(&mut self, node_id: DSTNodeId) {
    if let Some(&runner_idx) = self.runner_index_by_node.get(&node_id) {
      self.mark_runner_index_ready(runner_idx);
    }
  }

  fn mark_runner_index_ready(&mut self, runner_idx: usize) {
    let runner = &mut self.runners[runner_idx];
    if !runner.queued_for_pump {
      runner.queued_for_pump = true;
      self.ready_runners.push_back(runner_idx);
    }
  }
}

fn create_dst_backend(dst: &mut DST) -> DSTBackend {
  let node_id;
  let node = Rc::new(RefCell::new(DSTNodeState::default()));
  {
    let mut inner = dst.inner.borrow_mut();
    node_id = DSTNodeId(inner.next_node_id);
    inner.next_node_id = inner.next_node_id.saturating_add(1);
    let expected_idx = node_id.0 as usize;
    assert_eq!(
      inner.nodes.len(),
      expected_idx,
      "DST node ids must remain dense and sequential",
    );
    inner.nodes.push(node.clone());
    inner.node_faults.insert(
      node_id,
      DSTNodeFaultRuntime {
        policy: DSNodeFaults::Off,
        evaluated_bucket: u64::MAX,
        isolated_until_tick: 0,
      },
    );
  }

  let mut local_config = dst.config;
  // Shared DST owns time; keep delegated local ops immediate.
  local_config.max_delay_ticks = 0;

  DSTBackend {
    local: Some(DSBackend::with_config(local_config)),
    node_id,
    node,
    inner: dst.inner.clone(),
    queued: Vec::new(),
  }
}

impl Default for DST {
  fn default() -> Self {
    Self::new()
  }
}

impl DSTBackend {
  fn is_network_resource(&self, raw: i32) -> bool {
    self.node.borrow().sockets.contains_key(&raw)
  }

  fn queue_network_op(&mut self, id: u64, op: Op) {
    self.queued.push((id, op));
  }

  fn synthetic_peer_addr(
    domain: SockDomain,
    node_id: DSTNodeId,
    fd: i32,
  ) -> SocketAddrBuf {
    match domain {
      SockDomain::IPV4 => {
        let host_hi = ((node_id.0 % 200) + 20) as u8;
        let host_lo = (((fd as u64) % 200) + 20) as u8;
        let port = 30_000 + ((fd as u16) % 20_000);
        op::socket_addr_into_buf(
          format!("127.0.{}.{}:{}", host_hi, host_lo, port).parse().unwrap(),
        )
      }
      SockDomain::IPV6 => {
        let port = 30_000 + ((fd as u16) % 20_000);
        op::socket_addr_into_buf(format!("[::1]:{}", port).parse().unwrap())
      }
      _ => SocketAddrBuf::unspecified(),
    }
  }

  fn take_socket_payload(
    socket: &mut DSTSocketHandle,
    len_hint: usize,
    allow_pending: bool,
  ) -> Result<PendingResult<Vec<u8>>, isize> {
    if socket.local_shutdown_read {
      return Ok(PendingResult::Ready(Vec::new()));
    }

    match &mut socket.recv_queue {
      DSTRecvQueue::Stream(inbox) => {
        if inbox.is_empty() {
          if let Some(errno) = socket.peer_reset_error {
            return Err(-(errno as isize));
          }
          if socket.peer_write_closed {
            return Ok(PendingResult::Ready(Vec::new()));
          }
          if allow_pending
            && matches!(
              socket.state,
              DSTSocketState::Connected | DSTSocketState::Connecting
            )
          {
            return Ok(PendingResult::Pending);
          }
          return Err(-(libc::ENOTCONN as isize));
        }
        let take = inbox.len().min(len_hint.max(1));
        let mut out = Vec::with_capacity(take);
        for _ in 0..take {
          if let Some(byte) = inbox.pop_front() {
            out.push(byte);
          }
        }
        Ok(PendingResult::Ready(out))
      }
      DSTRecvQueue::Datagram(queue) => {
        if let Some(packet) = queue.pop_front() {
          let take = packet.len().min(len_hint.max(1));
          return Ok(PendingResult::Ready(packet[..take].to_vec()));
        }
        if allow_pending && socket.can_receive_pending() {
          return Ok(PendingResult::Pending);
        }
        Err(-(libc::EAGAIN as isize))
      }
    }
  }

  fn drain_node_ready(&mut self, completed: &mut Vec<OpCompleted>) {
    let mut node = self.node.borrow_mut();
    completed.append(&mut node.ready);
  }

  fn allocate_socket_fd(node: &mut DSTNodeState) -> i32 {
    let fd = node.next_fd;
    node.next_fd = node.next_fd.saturating_add(1);
    fd
  }

  fn allocate_network_seq(node: &mut DSTNodeState) -> u64 {
    let seq = node.next_network_seq;
    node.next_network_seq = node.next_network_seq.saturating_add(1);
    seq
  }

  #[allow(dead_code)]
  fn schedule_socket_reset(&mut self, peer: DSTSocketRef, errno: i32) {
    let source_seq = {
      let mut node = self.node.borrow_mut();
      Self::allocate_network_seq(&mut node)
    };
    let mut inner = self.inner.borrow_mut();
    let ready_at_tick =
      inner.tick + inner.event_delay_ticks_for(self.node_id, source_seq, 4);
    inner.enqueue_event(DSTEvent::SocketReset {
      ready_at_tick,
      source_node: self.node_id,
      source_seq,
      to: peer,
      errno,
    });
  }

  fn schedule_stream_link_reset(
    &mut self,
    local: DSTSocketRef,
    peer: DSTSocketRef,
    errno: i32,
  ) {
    self.schedule_socket_reset(local, errno);
    self.schedule_socket_reset(peer, errno);
  }

  fn link_is_available(&self, peer: DSTSocketRef) -> bool {
    self.inner.borrow_mut().link_is_available(self.node_id, peer.node_id)
  }

  fn write_to_msg_recv(msg: MsgRecv, bytes: &[u8]) -> usize {
    // SAFETY: op model keeps the descriptor array alive for the op lifetime.
    let bufs = unsafe {
      std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
    };
    let mut written = 0usize;
    for buf in bufs {
      if written == bytes.len() {
        break;
      }
      let chunk = (bytes.len() - written).min(buf.len);
      // SAFETY: caller provided live writable payload buffers for the op lifetime.
      let out =
        unsafe { std::slice::from_raw_parts_mut(buf.ptr.as_ptr(), chunk) };
      out.copy_from_slice(&bytes[written..written + chunk]);
      written += chunk;
    }
    written
  }

  fn process_network_read(
    node: &mut DSTNodeState,
    fd: &Resource,
    iovecs: NonNull<RawBuf>,
    iov_count: usize,
    offset: i64,
    flags: ReadFlags,
  ) -> PendingAction {
    if offset < -1 {
      return PendingAction::Complete(-(libc::EINVAL as isize));
    }
    if flags.bits() < 0 {
      return PendingAction::Complete(-(libc::ENOTSUP as isize));
    }
    if offset >= 0 {
      return PendingAction::Complete(-(libc::ESPIPE as isize));
    }
    let raw = DSBackend::resource_key(fd);
    let Some(socket) = node.sockets.get_mut(&raw) else {
      return PendingAction::Complete(-(libc::EBADF as isize));
    };
    let total: usize =
      // SAFETY: read-only access to caller-provided iovec metadata.
      unsafe { std::slice::from_raw_parts(iovecs.as_ptr(), iov_count) }
        .iter()
        .map(|iov| iov.len)
        .sum();
    match Self::take_socket_payload(socket, total, true) {
      Ok(PendingResult::Ready(bytes)) => PendingAction::Complete(
        DSBackend::write_to_iovecs(iovecs, iov_count, &bytes) as isize,
      ),
      Ok(PendingResult::Pending) => PendingAction::KeepPending(Op::Read {
        fd: fd.clone(),
        iovecs,
        iov_count,
        offset,
        flags,
      }),
      Err(err) => PendingAction::Complete(err),
    }
  }

  fn process_network_recv(
    node: &mut DSTNodeState,
    fd: &Resource,
    msg: MsgRecv,
    flags: RecvFlags,
  ) -> PendingAction {
    if flags.bits() < 0 {
      return PendingAction::Complete(-(libc::EINVAL as isize));
    }
    let raw = DSBackend::resource_key(fd);
    let Some(socket) = node.sockets.get_mut(&raw) else {
      return PendingAction::Complete(-(libc::EBADF as isize));
    };
    if let Some(out) = msg.from {
      let addr = socket.peer_addr.unwrap_or_else(SocketAddrBuf::unspecified);
      unsafe {
        out.as_ptr().write(addr);
      }
    }
    let total = unsafe {
      std::slice::from_raw_parts(msg.bufs.as_ptr(), msg.buf_count.get())
    }
    .iter()
    .map(|buf| buf.len)
    .sum();
    match Self::take_socket_payload(socket, total, true) {
      Ok(PendingResult::Ready(bytes)) => {
        PendingAction::Complete(Self::write_to_msg_recv(msg, &bytes) as isize)
      }
      Ok(PendingResult::Pending) => {
        PendingAction::KeepPending(Op::Recv { fd: fd.clone(), msg, flags })
      }
      Err(err) => PendingAction::Complete(err),
    }
  }

  fn process_network_accept(
    node: &mut DSTNodeState,
    fd: &Resource,
    addr: NonNull<SocketAddrBuf>,
  ) -> PendingAction {
    let raw = DSBackend::resource_key(fd);
    let Some(listener) = node.sockets.get_mut(&raw) else {
      return PendingAction::Complete(-(libc::EBADF as isize));
    };
    if listener.state != DSTSocketState::Listening {
      return PendingAction::Complete(-(libc::EINVAL as isize));
    }
    let Some(accepted_fd) = listener.pending_accept.pop_front() else {
      return PendingAction::KeepPending(Op::Accept { fd: fd.clone(), addr });
    };
    let peer_addr = node
      .sockets
      .get(&accepted_fd)
      .and_then(|socket| socket.peer_addr)
      .unwrap_or_else(SocketAddrBuf::unspecified);
    // SAFETY: op model keeps the output slot valid until completion.
    unsafe {
      addr.as_ptr().write(peer_addr);
    }
    PendingAction::Complete(accepted_fd as isize)
  }

  fn poll_pending_node_state(node: Rc<RefCell<DSTNodeState>>) {
    let mut node = node.borrow_mut();
    let pending = mem::take(&mut node.pending);
    node.pending.reserve(pending.len());
    for (id, op) in pending {
      let action = match op {
        Op::Read { fd, iovecs, iov_count, offset, flags } => {
          Self::process_network_read(
            &mut node, &fd, iovecs, iov_count, offset, flags,
          )
        }
        Op::Recv { fd, msg, flags } => {
          Self::process_network_recv(&mut node, &fd, msg, flags)
        }
        Op::Accept { fd, addr } => {
          Self::process_network_accept(&mut node, &fd, addr)
        }
        other => PendingAction::Complete(match other {
          Op::Nop => 0,
          _ => -(libc::EINVAL as isize),
        }),
      };
      match action {
        PendingAction::Complete(result) => {
          node.ready.push(OpCompleted::new(id, result));
        }
        PendingAction::KeepPending(op) => {
          node.pending.push((id, op));
        }
      }
    }
  }

  fn process_network_write(
    &mut self,
    fd: &Resource,
    bytes: &[u8],
    offset: Option<usize>,
    to: Option<SocketAddrBuf>,
  ) -> Result<usize, isize> {
    let raw = DSBackend::resource_key(fd);
    if offset.is_some() {
      return Err(-(libc::ESPIPE as isize));
    }

    let (peer, ty, state, domain, bound) = {
      let mut node = self.node.borrow_mut();
      let Some(socket) = node.sockets.get_mut(&raw) else {
        return Err(-(libc::EBADF as isize));
      };
      if let Some(errno) = socket.peer_reset_error {
        return Err(-(errno as isize));
      }
      if socket.local_shutdown_write {
        return Err(-(libc::EPIPE as isize));
      }
      (socket.peer, socket.ty, socket.state, socket.domain, socket.bound)
    };

    let source_seq = {
      let mut node = self.node.borrow_mut();
      Self::allocate_network_seq(&mut node)
    };

    match ty {
      SockType::STREAM => {
        if state != DSTSocketState::Connected {
          return Err(-(libc::ENOTCONN as isize));
        }
        let Some(peer_fd) = peer else {
          return Err(-(libc::ENOTCONN as isize));
        };
        if !self.link_is_available(peer_fd) {
          self.schedule_stream_link_reset(
            DSTSocketRef { node_id: self.node_id, fd: raw },
            peer_fd,
            libc::ECONNRESET,
          );
          return Err(-(libc::EHOSTUNREACH as isize));
        }
        let mut inner = self.inner.borrow_mut();
        let base_ready_at_tick =
          inner.tick + inner.event_delay_ticks_for(self.node_id, source_seq, 1);
        let ready_at_tick = {
          let mut node = self.node.borrow_mut();
          let Some(socket) = node.sockets.get_mut(&raw) else {
            return Err(-(libc::EBADF as isize));
          };
          let ready_at_tick =
            base_ready_at_tick.max(socket.next_stream_ready_at_tick);
          socket.next_stream_ready_at_tick = ready_at_tick;
          ready_at_tick
        };
        inner.enqueue_event(DSTEvent::StreamDelivered {
          ready_at_tick,
          source_node: self.node_id,
          source_seq,
          from: DSTSocketRef { node_id: self.node_id, fd: raw },
          to: DSTSocketRef { node_id: peer_fd.node_id, fd: peer_fd.fd },
          bytes: bytes.to_vec(),
        });
      }
      SockType::DGRAM => {
        if matches!(state, DSTSocketState::Closed) {
          return Err(-(libc::ENOTCONN as isize));
        }
        let peer_fd = if let Some(addr) = to {
          let key = DSBackend::socket_addr_key(&addr);
          let inner = self.inner.borrow();
          let peer =
            inner.bound.get(&key).copied().ok_or(-(libc::ENOENT as isize))?;
          let is_dgram = inner
            .node(peer.node_id)
            .borrow()
            .sockets
            .get(&peer.fd)
            .is_some_and(|socket| socket.ty == SockType::DGRAM);
          if !is_dgram {
            return Err(-(libc::EPROTOTYPE as isize));
          }
          peer
        } else {
          peer.ok_or(-(libc::EDESTADDRREQ as isize))?
        };
        if !self.link_is_available(peer_fd) {
          return Err(-(libc::EHOSTUNREACH as isize));
        }
        let mut inner = self.inner.borrow_mut();
        let ready_at_tick =
          inner.tick + inner.event_delay_ticks_for(self.node_id, source_seq, 2);
        let from_addr = bound.unwrap_or_else(|| {
          Self::synthetic_peer_addr(domain, self.node_id, raw)
        });
        inner.enqueue_event(DSTEvent::DatagramDelivered {
          ready_at_tick,
          source_node: self.node_id,
          source_seq,
          from: DSTSocketRef { node_id: self.node_id, fd: raw },
          from_addr,
          to: peer_fd,
          packet: bytes.to_vec(),
        });
      }
      _ => return Err(-(libc::ENOTSUP as isize)),
    }
    Ok(bytes.len())
  }

  fn network_complete_op(&mut self, id: u64, op: Op) {
    match op {
      Op::Socket { domain, ty, proto } => {
        if op::socket_to_raw(domain, ty, proto).is_err() {
          self
            .node
            .borrow_mut()
            .ready
            .push(OpCompleted::new(id, -(libc::EINVAL as isize)));
          return;
        }
        let fd = {
          let mut node = self.node.borrow_mut();
          let fd = Self::allocate_socket_fd(&mut node);
          node.sockets.insert(fd, DSTSocketHandle::new(domain, ty, proto));
          fd
        };
        self.node.borrow_mut().ready.push(OpCompleted::new(id, fd as isize));
      }
      Op::Bind { fd, addr } => {
        let raw = DSBackend::resource_key(&fd);
        let addr = op::socket_addr_into_buf(addr);
        let key = DSBackend::socket_addr_key(&addr);
        let mut node = self.node.borrow_mut();
        let Some(socket) = node.sockets.get_mut(&raw) else {
          node.ready.push(OpCompleted::new(id, -(libc::EBADF as isize)));
          return;
        };
        let mut inner = self.inner.borrow_mut();
        if inner.bound.contains_key(&key)
          || inner.listeners.contains_key(&key)
          || inner.occupied_addr_keys.iter().any(|occupied| occupied == &key)
        {
          node.ready.push(OpCompleted::new(id, -(libc::EADDRINUSE as isize)));
          return;
        }
        socket.bound = Some(addr);
        socket.state = DSTSocketState::Bound;
        inner
          .bound
          .insert(key, DSTSocketRef { node_id: self.node_id, fd: raw });
        node.ready.push(OpCompleted::new(id, 0));
      }
      Op::Listen { fd, backlog } => {
        let raw = DSBackend::resource_key(&fd);
        let mut node = self.node.borrow_mut();
        let Some(socket) = node.sockets.get_mut(&raw) else {
          node.ready.push(OpCompleted::new(id, -(libc::EBADF as isize)));
          return;
        };
        if socket.ty != SockType::STREAM {
          node.ready.push(OpCompleted::new(id, -(libc::EOPNOTSUPP as isize)));
          return;
        }
        let Some(bound) = socket.bound else {
          node.ready.push(OpCompleted::new(id, -(libc::EINVAL as isize)));
          return;
        };
        let key = DSBackend::socket_addr_key(&bound);
        socket.state = DSTSocketState::Listening;
        socket.backlog = backlog.max(0);
        let mut inner = self.inner.borrow_mut();
        inner
          .listeners
          .insert(key, DSTSocketRef { node_id: self.node_id, fd: raw });
        node.ready.push(OpCompleted::new(id, 0));
      }
      Op::Connect { fd, addr } => {
        let raw = DSBackend::resource_key(&fd);
        let key = DSBackend::socket_addr_key(&addr);
        let (domain, ty, proto, client_bound, client_state) = {
          let node = self.node.borrow();
          let Some(socket) = node.sockets.get(&raw) else {
            drop(node);
            self
              .node
              .borrow_mut()
              .ready
              .push(OpCompleted::new(id, -(libc::EBADF as isize)));
            return;
          };
          (socket.domain, socket.ty, socket.proto, socket.bound, socket.state)
        };

        match ty {
          SockType::STREAM => {
            if !matches!(
              client_state,
              DSTSocketState::Created | DSTSocketState::Bound
            ) {
              self
                .node
                .borrow_mut()
                .ready
                .push(OpCompleted::new(id, -(libc::EISCONN as isize)));
              return;
            }
            let listener_ref = {
              let inner = self.inner.borrow();
              inner.listeners.get(&key).copied()
            };
            let Some(listener_ref) = listener_ref else {
              self
                .node
                .borrow_mut()
                .ready
                .push(OpCompleted::new(id, -(libc::ENOENT as isize)));
              return;
            };
            if !self.link_is_available(listener_ref) {
              self
                .node
                .borrow_mut()
                .ready
                .push(OpCompleted::new(id, -(libc::EHOSTUNREACH as isize)));
              return;
            }
            let client_peer_addr = client_bound.unwrap_or_else(|| {
              Self::synthetic_peer_addr(domain, self.node_id, raw)
            });

            let listener_node = self.inner.borrow().node(listener_ref.node_id);
            let server_fd = {
              let mut listener_node = listener_node.borrow_mut();
              let Some(listener_socket) =
                listener_node.sockets.get_mut(&listener_ref.fd)
              else {
                self
                  .node
                  .borrow_mut()
                  .ready
                  .push(OpCompleted::new(id, -(libc::ECONNREFUSED as isize)));
                return;
              };
              let backlog_limit = listener_socket.backlog.max(0) as usize;
              if listener_socket.state != DSTSocketState::Listening
                || backlog_limit == 0
                || listener_socket.pending_accept.len()
                  + listener_socket.backlog_reserved
                  >= backlog_limit
              {
                self
                  .node
                  .borrow_mut()
                  .ready
                  .push(OpCompleted::new(id, -(libc::ECONNREFUSED as isize)));
                return;
              }
              listener_socket.backlog_reserved += 1;
              let server_fd = Self::allocate_socket_fd(&mut listener_node);
              let mut accepted = DSTSocketHandle::new(domain, ty, proto);
              accepted.state = DSTSocketState::Connected;
              accepted.peer_addr = Some(client_peer_addr);
              accepted.peer =
                Some(DSTSocketRef { node_id: self.node_id, fd: raw });
              listener_node.sockets.insert(server_fd, accepted);
              server_fd
            };

            let source_seq = {
              let mut node = self.node.borrow_mut();
              Self::allocate_network_seq(&mut node)
            };
            {
              let mut inner = self.inner.borrow_mut();
              let ready_at_tick = inner.tick
                + inner.event_delay_ticks_for(self.node_id, source_seq, 0);
              inner.enqueue_event(DSTEvent::ConnectEstablished {
                ready_at_tick,
                source_node: self.node_id,
                source_seq,
                connect_id: id,
                client: DSTSocketRef { node_id: self.node_id, fd: raw },
                listener: listener_ref,
                server_fd,
                server_peer_addr: client_peer_addr,
              });
            }

            let mut node = self.node.borrow_mut();
            if let Some(socket) = node.sockets.get_mut(&raw) {
              socket.state = DSTSocketState::Connecting;
              socket.peer_addr = Some(addr);
            }
          }
          SockType::DGRAM => {
            let peer_ref = {
              let inner = self.inner.borrow();
              inner.bound.get(&key).copied()
            };
            let Some(peer_ref) = peer_ref else {
              self
                .node
                .borrow_mut()
                .ready
                .push(OpCompleted::new(id, -(libc::ENOENT as isize)));
              return;
            };
            if !self.link_is_available(peer_ref) {
              self
                .node
                .borrow_mut()
                .ready
                .push(OpCompleted::new(id, -(libc::EHOSTUNREACH as isize)));
              return;
            }
            let peer_is_dgram = self
              .inner
              .borrow()
              .node(peer_ref.node_id)
              .borrow()
              .sockets
              .get(&peer_ref.fd)
              .is_some_and(|socket| socket.ty == SockType::DGRAM);
            if !peer_is_dgram {
              self
                .node
                .borrow_mut()
                .ready
                .push(OpCompleted::new(id, -(libc::EPROTOTYPE as isize)));
              return;
            }
            let mut node = self.node.borrow_mut();
            let Some(socket) = node.sockets.get_mut(&raw) else {
              node.ready.push(OpCompleted::new(id, -(libc::EBADF as isize)));
              return;
            };
            socket.state = DSTSocketState::Connected;
            socket.peer = Some(peer_ref);
            socket.peer_addr = Some(addr);
            node.ready.push(OpCompleted::new(id, 0));
          }
          _ => {
            self
              .node
              .borrow_mut()
              .ready
              .push(OpCompleted::new(id, -(libc::ENOTSUP as isize)));
          }
        }
      }
      Op::Send { fd, msg, flags } => {
        if flags.bits() < 0 {
          self
            .node
            .borrow_mut()
            .ready
            .push(OpCompleted::new(id, -(libc::EINVAL as isize)));
          return;
        }
        let target = msg.to.map(op::socket_addr_into_buf);
        let bytes = DSBackend::read_msg_send(msg);
        let result = self.process_network_write(&fd, &bytes, None, target);
        self.node.borrow_mut().ready.push(OpCompleted::new(
          id,
          result.map(|len| len as isize).unwrap_or_else(|err| err),
        ));
      }
      Op::Write { fd, iovecs, iov_count, offset, flags } => {
        if offset < -1 {
          self
            .node
            .borrow_mut()
            .ready
            .push(OpCompleted::new(id, -(libc::EINVAL as isize)));
          return;
        }
        if flags.bits() < 0 {
          self
            .node
            .borrow_mut()
            .ready
            .push(OpCompleted::new(id, -(libc::ENOTSUP as isize)));
          return;
        }
        let bytes = DSBackend::extract_iovec_bytes(iovecs, iov_count);
        let result = self.process_network_write(
          &fd,
          &bytes,
          (offset >= 0).then_some(offset as usize),
          None,
        );
        self.node.borrow_mut().ready.push(OpCompleted::new(
          id,
          result.map(|len| len as isize).unwrap_or_else(|err| err),
        ));
      }
      Op::Recv { fd, msg, flags } => {
        let mut node = self.node.borrow_mut();
        let action = Self::process_network_recv(&mut node, &fd, msg, flags);
        match action {
          PendingAction::Complete(result) => {
            node.ready.push(OpCompleted::new(id, result));
          }
          PendingAction::KeepPending(op) => node.pending.push((id, op)),
        }
      }
      Op::Read { fd, iovecs, iov_count, offset, flags } => {
        let mut node = self.node.borrow_mut();
        let action = Self::process_network_read(
          &mut node, &fd, iovecs, iov_count, offset, flags,
        );
        match action {
          PendingAction::Complete(result) => {
            node.ready.push(OpCompleted::new(id, result));
          }
          PendingAction::KeepPending(op) => node.pending.push((id, op)),
        }
      }
      Op::Accept { fd, addr } => {
        let mut node = self.node.borrow_mut();
        let action = Self::process_network_accept(&mut node, &fd, addr);
        match action {
          PendingAction::Complete(result) => {
            node.ready.push(OpCompleted::new(id, result));
          }
          PendingAction::KeepPending(op) => node.pending.push((id, op)),
        }
      }
      Op::Shutdown { fd, how } => {
        let raw = DSBackend::resource_key(&fd);
        let peer = {
          let mut node = self.node.borrow_mut();
          let Some(socket) = node.sockets.get_mut(&raw) else {
            node.ready.push(OpCompleted::new(id, -(libc::EBADF as isize)));
            return;
          };
          match how {
            ShutdownHow::Read => socket.local_shutdown_read = true,
            ShutdownHow::Write => socket.local_shutdown_write = true,
            ShutdownHow::Both => {
              socket.local_shutdown_read = true;
              socket.local_shutdown_write = true;
              socket.state = DSTSocketState::Closed;
            }
          }
          socket.peer
        };

        if matches!(how, ShutdownHow::Write | ShutdownHow::Both)
          && let Some(peer) = peer
          && self
            .node
            .borrow()
            .sockets
            .get(&raw)
            .is_some_and(|socket| socket.ty == SockType::STREAM)
        {
          let source_seq = {
            let mut node = self.node.borrow_mut();
            Self::allocate_network_seq(&mut node)
          };
          {
            let mut inner = self.inner.borrow_mut();
            let ready_at_tick = inner.tick
              + inner.event_delay_ticks_for(self.node_id, source_seq, 2);
            inner.enqueue_event(DSTEvent::SocketShutdownRead {
              ready_at_tick,
              source_node: self.node_id,
              source_seq,
              to: peer,
            });
          }
        }
        self.node.borrow_mut().ready.push(OpCompleted::new(id, 0));
      }
      Op::Fsync { .. } => {
        self
          .node
          .borrow_mut()
          .ready
          .push(OpCompleted::new(id, -(libc::EINVAL as isize)));
      }
      other => {
        self
          .local
          .as_mut()
          .expect("non-network DST backend op requires local backend")
          .push(id, other, &mut Bump::new());
      }
    }
  }
}

impl IoBackend for DSTBackend {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self
      .local
      .as_mut()
      .expect("DST backend init requires local backend")
      .init(cap)?;
    self.queued = Vec::with_capacity(cap);
    let mut node = self.node.borrow_mut();
    node.initialized = true;
    node.cap = cap;
    node.next_fd = DST_SOCKET_RESOURCE_BASE;
    node.next_network_seq = 0;
    node.sockets.clear();
    node.outgoing.clear();
    node.pending.clear();
    node.ready.clear();
    self.inner.borrow_mut().active_outgoing_nodes.remove(&self.node_id);
    Ok(())
  }

  fn push(&mut self, id: u64, op: Op, step_bump: &mut Bump) {
    let use_network = match &op {
      Op::Socket { .. } => true,
      Op::Bind { fd, .. }
      | Op::Listen { fd, .. }
      | Op::Connect { fd, .. }
      | Op::Accept { fd, .. }
      | Op::Send { fd, .. }
      | Op::Recv { fd, .. }
      | Op::Shutdown { fd, .. }
      | Op::Fsync { fd } => {
        self.is_network_resource(DSBackend::resource_key(fd))
      }
      Op::Read { fd, .. } | Op::Write { fd, .. } => {
        self.is_network_resource(DSBackend::resource_key(fd))
      }
      _ => false,
    };

    if use_network {
      self.queue_network_op(id, op);
    } else {
      self
        .local
        .as_mut()
        .expect("non-network DST backend push requires local backend")
        .push(id, op, step_bump);
    }
  }

  fn flush(&mut self) -> io::Result<()> {
    self
      .local
      .as_mut()
      .expect("DST backend flush requires local backend")
      .flush()?;
    let queued = mem::take(&mut self.queued);
    if !queued.is_empty() {
      self.node.borrow_mut().outgoing.extend(queued);
      self.inner.borrow_mut().active_outgoing_nodes.insert(self.node_id);
    }
    Ok(())
  }

  fn wait(
    &mut self,
    _timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    completed.clear();
    self.drain_node_ready(completed);

    let mut local_completed = Vec::new();
    self
      .local
      .as_mut()
      .expect("DST backend wait requires local backend")
      .wait(Some(Duration::ZERO), &mut local_completed)?;
    completed.append(&mut local_completed);
    Ok(())
  }
}

// #[cfg(test)]
// mod test_io_backend {
//   lio_test::test_io_backend!(super::DSBackend::new());
// }

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::op::{MsgBufMut, MsgRecv};
  use std::net::SocketAddr;

  fn dst_test_socket(node: &Rc<RefCell<DSTNodeState>>, fd: i32) {
    let mut socket =
      DSTSocketHandle::new(SockDomain::IPV4, SockType::STREAM, SockProto::TCP);
    socket.state = DSTSocketState::Connected;
    node.borrow_mut().sockets.insert(fd, socket);
  }

  fn dst_stream_queue_bytes(socket: &DSTSocketHandle) -> Vec<u8> {
    match &socket.recv_queue {
      DSTRecvQueue::Stream(inbox) => inbox.iter().copied().collect(),
      DSTRecvQueue::Datagram(_) => panic!("expected stream recv queue"),
    }
  }

  fn dst_take_ready(node: &Rc<RefCell<DSTNodeState>>) -> Vec<OpCompleted> {
    let mut node = node.borrow_mut();
    std::mem::take(&mut node.ready)
  }

  fn dst_resource(fd: i32) -> Resource {
    // DS tests use simulated descriptor numbers managed by DSBackend, not the
    // process fd table. Borrow them so dropping Resource never closes an
    // unrelated real fd owned by a concurrently running test.
    unsafe { Resource::borrow(fd) }
  }

  fn dst_connect_completion_tick(seed: u64) -> u64 {
    let mut dst = DST::with_config(DSConfig {
      seed,
      max_delay_ticks: 2,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    let mut listener_backend = create_dst_backend(&mut dst);
    let mut client_backend = create_dst_backend(&mut dst);

    listener_backend.network_complete_op(
      1,
      Op::Socket {
        domain: SockDomain::IPV4,
        ty: SockType::STREAM,
        proto: SockProto::TCP,
      },
    );
    let listener_fd = dst_take_ready(&listener_backend.node)[0].result() as i32;
    let listener = dst_resource(listener_fd);
    let addr: SocketAddr = "127.0.0.1:7020".parse().unwrap();
    listener_backend
      .network_complete_op(2, Op::Bind { fd: listener.clone(), addr });
    let _ = dst_take_ready(&listener_backend.node);
    listener_backend
      .network_complete_op(3, Op::Listen { fd: listener, backlog: 1 });
    let _ = dst_take_ready(&listener_backend.node);

    client_backend.network_complete_op(
      4,
      Op::Socket {
        domain: SockDomain::IPV4,
        ty: SockType::STREAM,
        proto: SockProto::TCP,
      },
    );
    let client_fd = dst_take_ready(&client_backend.node)[0].result() as i32;
    client_backend.network_complete_op(
      5,
      Op::Connect {
        fd: dst_resource(client_fd),
        addr: op::socket_addr_into_buf(addr),
      },
    );
    assert!(dst_take_ready(&client_backend.node).is_empty());

    for tick in 0..16 {
      if dst.step() {
        let ready = dst_take_ready(&client_backend.node);
        if ready.iter().any(|completion| completion.registration_id() == 5) {
          return tick;
        }
      }
      let _ = dst.tick();
      let ready = dst_take_ready(&client_backend.node);
      if ready.iter().any(|completion| completion.registration_id() == 5) {
        return tick + 1;
      }
    }

    panic!("connect did not complete within tick budget");
  }

  fn dst_delivery_order_for_seed(seed: u64) -> Vec<u8> {
    let mut dst = DST::with_config(DSConfig {
      seed,
      max_delay_ticks: 0,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    let sender_a = create_dst_backend(&mut dst);
    let sender_b = create_dst_backend(&mut dst);
    let receiver = create_dst_backend(&mut dst);

    dst_test_socket(&receiver.node, 77);

    {
      let mut inner = dst.inner.borrow_mut();
      inner.enqueue_event(DSTEvent::StreamDelivered {
        ready_at_tick: 0,
        source_node: sender_a.node_id,
        source_seq: 0,
        from: DSTSocketRef { node_id: sender_a.node_id, fd: 11 },
        to: DSTSocketRef { node_id: receiver.node_id, fd: 77 },
        bytes: b"a".to_vec(),
      });
      inner.enqueue_event(DSTEvent::StreamDelivered {
        ready_at_tick: 0,
        source_node: sender_b.node_id,
        source_seq: 0,
        from: DSTSocketRef { node_id: sender_b.node_id, fd: 22 },
        to: DSTSocketRef { node_id: receiver.node_id, fd: 77 },
        bytes: b"b".to_vec(),
      });
    }

    assert!(dst.step());
    receiver.node.borrow().sockets.get(&77).map(dst_stream_queue_bytes).unwrap()
  }

  #[test]
  fn nop_is_deterministic_for_a_seed() {
    let mut a = DSBackend::with_seed(42);
    let mut b = DSBackend::with_seed(42);
    a.init(8).unwrap();
    b.init(8).unwrap();

    let mut step_a = Bump::new();
    let mut step_b = Bump::new();
    a.push(1, Op::Nop, &mut step_a);
    b.push(1, Op::Nop, &mut step_b);
    a.flush().unwrap();
    b.flush().unwrap();

    let mut done_a = Vec::new();
    let mut done_b = Vec::new();
    a.wait(None, &mut done_a).unwrap();
    b.wait(None, &mut done_b).unwrap();

    assert_eq!(done_a.len(), 1);
    assert_eq!(done_b.len(), 1);
    assert_eq!(done_a[0].result(), done_b[0].result());
  }

  #[test]
  fn getcwd_writes_simulated_path() {
    let mut backend = DSBackend::with_config(DSConfig {
      seed: 7,
      max_delay_ticks: 0,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    backend.init(8).unwrap();

    let mut out = std::ffi::OsString::new();
    let mut step_bump = Bump::new();
    backend.push(
      1,
      Op::GetCwd { out: NonNull::from(&mut out) },
      &mut step_bump,
    );
    backend.flush().unwrap();

    let mut completed = Vec::new();
    backend.wait(None, &mut completed).unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].result(), 0);
    #[cfg(unix)]
    {
      use std::os::unix::ffi::OsStrExt;
      assert_eq!(out.as_os_str().as_bytes(), backend.scenario.cwd.as_slice());
    }
  }

  #[test]
  fn recv_on_stdin_is_not_supported() {
    let mut backend = DSBackend::with_config(DSConfig {
      seed: 9,
      max_delay_ticks: 0,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    backend.init(8).unwrap();

    let mut buf = [0_u8; 16];
    let mut msg_buf = MsgBufMut::from_slice(&mut buf);
    let msg = MsgRecv::new(std::slice::from_mut(&mut msg_buf));
    let mut step_bump = Bump::new();
    backend.push(
      1,
      Op::Recv { fd: Resource::stdin(), msg, flags: RecvFlags::EMPTY },
      &mut step_bump,
    );
    backend.flush().unwrap();

    let mut completed = Vec::new();
    backend.wait(None, &mut completed).unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].result(), -(libc::ENOTSUP as isize));
  }

  #[test]
  fn recv_on_empty_socket_is_not_supported() {
    let mut backend = DSBackend::with_config(DSConfig {
      seed: 11,
      max_delay_ticks: 0,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    backend.init(8).unwrap();

    let socket_fd = backend.create_socket_resource(
      SockDomain::IPV4,
      SockType::STREAM,
      SockProto::TCP,
    );
    let socket = dst_resource(socket_fd);

    let mut buf = [0_u8; 16];
    let mut msg_buf = MsgBufMut::from_slice(&mut buf);
    let msg = MsgRecv::new(std::slice::from_mut(&mut msg_buf));
    let mut step_bump = Bump::new();
    backend.push(
      1,
      Op::Recv { fd: socket, msg, flags: RecvFlags::EMPTY },
      &mut step_bump,
    );
    backend.flush().unwrap();

    let mut completed = Vec::new();
    backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].result(), -(libc::ENOTSUP as isize));
  }

  #[test]
  fn recv_on_socket_reads_only_bytes_sent_by_peer() {
    let mut backend = DSBackend::with_config(DSConfig {
      seed: 13,
      max_delay_ticks: 0,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    backend.init(16).unwrap();

    let sender_fd = backend.create_socket_resource(
      SockDomain::IPV4,
      SockType::STREAM,
      SockProto::TCP,
    );
    let receiver_fd = backend.create_socket_resource(
      SockDomain::IPV4,
      SockType::STREAM,
      SockProto::TCP,
    );

    if let Some(SimResource::Socket(sender)) =
      backend.world.resources.get_mut(&sender_fd)
    {
      sender.peer = Some(receiver_fd);
    } else {
      panic!("sender socket resource missing");
    }
    if let Some(SimResource::Socket(receiver)) =
      backend.world.resources.get_mut(&receiver_fd)
    {
      receiver.peer = Some(sender_fd);
    } else {
      panic!("receiver socket resource missing");
    }

    let sender = dst_resource(sender_fd);
    let receiver = dst_resource(receiver_fd);

    let payload = b"ping".to_vec();
    let send_buf = crate::backend::op::MsgBuf::from_slice(&payload);
    let send_msg = MsgSend::new(std::slice::from_ref(&send_buf), None);
    let mut send_bump = Bump::new();
    backend.push(
      1,
      Op::Send { fd: sender, msg: send_msg, flags: SendFlags::EMPTY },
      &mut send_bump,
    );
    backend.flush().unwrap();

    let mut send_done = Vec::new();
    backend.wait(Some(Duration::ZERO), &mut send_done).unwrap();
    assert_eq!(send_done.len(), 1);
    let sent = send_done[0].result();
    assert!(sent > 0);

    let mut buf = [0_u8; 16];
    let mut recv_buf = MsgBufMut::from_slice(&mut buf);
    let recv_msg = MsgRecv::new(std::slice::from_mut(&mut recv_buf));
    let mut recv_bump = Bump::new();
    backend.push(
      2,
      Op::Recv { fd: receiver, msg: recv_msg, flags: RecvFlags::EMPTY },
      &mut recv_bump,
    );
    backend.flush().unwrap();

    let mut recv_done = Vec::new();
    backend.wait(Some(Duration::ZERO), &mut recv_done).unwrap();
    assert_eq!(recv_done.len(), 1);
    let recv_len = recv_done[0].result() as usize;
    assert!(recv_len > 0);
    assert!(recv_len <= sent as usize);
    assert_eq!(&buf[..recv_len], &payload[..recv_len]);
  }

  #[test]
  fn recv_on_connected_empty_socket_waits_for_delivery() {
    let mut backend = DSBackend::with_config(DSConfig {
      seed: 17,
      max_delay_ticks: 0,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    backend.init(16).unwrap();

    let sender_fd = backend.create_socket_resource(
      SockDomain::IPV4,
      SockType::STREAM,
      SockProto::TCP,
    );
    let receiver_fd = backend.create_socket_resource(
      SockDomain::IPV4,
      SockType::STREAM,
      SockProto::TCP,
    );

    if let Some(SimResource::Socket(sender)) =
      backend.world.resources.get_mut(&sender_fd)
    {
      sender.peer = Some(receiver_fd);
    } else {
      panic!("sender socket resource missing");
    }
    if let Some(SimResource::Socket(receiver)) =
      backend.world.resources.get_mut(&receiver_fd)
    {
      receiver.peer = Some(sender_fd);
    } else {
      panic!("receiver socket resource missing");
    }

    let sender = dst_resource(sender_fd);
    let receiver = dst_resource(receiver_fd);

    let mut buf = [0_u8; 16];
    let mut recv_buf = MsgBufMut::from_slice(&mut buf);
    let recv_msg = MsgRecv::new(std::slice::from_mut(&mut recv_buf));
    let mut recv_bump = Bump::new();
    backend.push(
      1,
      Op::Recv { fd: receiver, msg: recv_msg, flags: RecvFlags::EMPTY },
      &mut recv_bump,
    );
    backend.flush().unwrap();

    let mut completed = Vec::new();
    backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
    assert!(
      completed.is_empty(),
      "connected empty recv should pend until bytes are delivered"
    );

    let payload = b"ping".to_vec();
    let send_buf = crate::backend::op::MsgBuf::from_slice(&payload);
    let send_msg = MsgSend::new(std::slice::from_ref(&send_buf), None);
    let mut send_bump = Bump::new();
    backend.push(
      2,
      Op::Send { fd: sender, msg: send_msg, flags: SendFlags::EMPTY },
      &mut send_bump,
    );
    backend.flush().unwrap();

    backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].registration_id(), 2);

    backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].registration_id(), 1);
    let recv_len = completed[0].result() as usize;
    assert!(recv_len > 0);
    assert_eq!(&buf[..recv_len], &payload[..recv_len]);
  }

  #[test]
  fn accept_waits_for_connect_event() {
    let mut backend = DSBackend::with_config(DSConfig {
      seed: 19,
      max_delay_ticks: 0,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    backend.init(16).unwrap();

    let listener_fd = backend.create_socket_resource(
      SockDomain::IPV4,
      SockType::STREAM,
      SockProto::TCP,
    );
    let client_fd = backend.create_socket_resource(
      SockDomain::IPV4,
      SockType::STREAM,
      SockProto::TCP,
    );

    if let Some(SimResource::Socket(listener)) =
      backend.world.resources.get_mut(&listener_fd)
    {
      listener.listening = true;
      listener.bound =
        Some(op::socket_addr_into_buf("127.0.0.1:7000".parse().unwrap()));
    } else {
      panic!("listener socket resource missing");
    }
    let listener_key = DSBackend::socket_addr_key(&op::socket_addr_into_buf(
      "127.0.0.1:7000".parse().unwrap(),
    ));
    backend.world.bound_listeners.insert(listener_key, listener_fd);

    let listener = dst_resource(listener_fd);
    let client = dst_resource(client_fd);

    let mut accepted_addr = SocketAddrBuf::unspecified();
    let mut accept_bump = Bump::new();
    backend.push(
      1,
      Op::Accept {
        fd: listener,
        addr: NonNull::new(&mut accepted_addr).unwrap(),
      },
      &mut accept_bump,
    );

    let connect_addr = "127.0.0.1:7000".parse().unwrap();
    let mut connect_bump = Bump::new();
    backend.push(
      2,
      Op::Connect { fd: client, addr: op::socket_addr_into_buf(connect_addr) },
      &mut connect_bump,
    );
    backend.flush().unwrap();

    let mut completed = Vec::new();
    backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
    assert_eq!(completed.len(), 2);
    let mut saw_accept = false;
    let mut saw_connect = false;
    for completion in &completed {
      match completion.registration_id() {
        1 => {
          saw_accept = true;
          assert!(completion.result() >= 0);
        }
        2 => {
          saw_connect = true;
          assert_eq!(completion.result(), 0);
        }
        other => panic!("unexpected completion id {other}"),
      }
    }
    assert!(saw_accept && saw_connect);
  }

  #[test]
  fn progress_snapshot_reflects_pending_connected_recv() {
    let mut backend = DSBackend::with_config(DSConfig {
      seed: 23,
      max_delay_ticks: 0,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    backend.init(16).unwrap();

    let sender_fd = backend.create_socket_resource(
      SockDomain::IPV4,
      SockType::STREAM,
      SockProto::TCP,
    );
    let receiver_fd = backend.create_socket_resource(
      SockDomain::IPV4,
      SockType::STREAM,
      SockProto::TCP,
    );

    if let Some(SimResource::Socket(sender)) =
      backend.world.resources.get_mut(&sender_fd)
    {
      sender.peer = Some(receiver_fd);
    } else {
      panic!("sender socket resource missing");
    }
    if let Some(SimResource::Socket(receiver)) =
      backend.world.resources.get_mut(&receiver_fd)
    {
      receiver.peer = Some(sender_fd);
    } else {
      panic!("receiver socket resource missing");
    }

    let receiver = dst_resource(receiver_fd);

    let mut buf = [0_u8; 16];
    let mut recv_buf = MsgBufMut::from_slice(&mut buf);
    let recv_msg = MsgRecv::new(std::slice::from_mut(&mut recv_buf));
    let before = backend.progress_snapshot();

    let mut recv_bump = Bump::new();
    backend.push(
      1,
      Op::Recv { fd: receiver, msg: recv_msg, flags: RecvFlags::EMPTY },
      &mut recv_bump,
    );
    backend.flush().unwrap();

    let after_flush = backend.progress_snapshot();
    assert_eq!(after_flush.queued, 0);
    assert_eq!(after_flush.pending, before.pending + 1);
    assert!(backend.has_pending_work());
    assert!(!backend.is_quiescent());
  }

  #[test]
  fn backend_becomes_quiescent_after_delivery_and_completion_drain() {
    let mut backend = DSBackend::with_config(DSConfig {
      seed: 29,
      max_delay_ticks: 0,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    backend.init(16).unwrap();

    let sender_fd = backend.create_socket_resource(
      SockDomain::IPV4,
      SockType::STREAM,
      SockProto::TCP,
    );
    let receiver_fd = backend.create_socket_resource(
      SockDomain::IPV4,
      SockType::STREAM,
      SockProto::TCP,
    );

    if let Some(SimResource::Socket(sender)) =
      backend.world.resources.get_mut(&sender_fd)
    {
      sender.peer = Some(receiver_fd);
    } else {
      panic!("sender socket resource missing");
    }
    if let Some(SimResource::Socket(receiver)) =
      backend.world.resources.get_mut(&receiver_fd)
    {
      receiver.peer = Some(sender_fd);
    } else {
      panic!("receiver socket resource missing");
    }

    let sender = dst_resource(sender_fd);
    let receiver = dst_resource(receiver_fd);

    let mut buf = [0_u8; 16];
    let mut recv_buf = MsgBufMut::from_slice(&mut buf);
    let recv_msg = MsgRecv::new(std::slice::from_mut(&mut recv_buf));
    let mut recv_bump = Bump::new();
    backend.push(
      1,
      Op::Recv { fd: receiver, msg: recv_msg, flags: RecvFlags::EMPTY },
      &mut recv_bump,
    );

    let payload = b"ping".to_vec();
    let send_buf = crate::backend::op::MsgBuf::from_slice(&payload);
    let send_msg = MsgSend::new(std::slice::from_ref(&send_buf), None);
    let mut send_bump = Bump::new();
    backend.push(
      2,
      Op::Send { fd: sender, msg: send_msg, flags: SendFlags::EMPTY },
      &mut send_bump,
    );
    backend.flush().unwrap();

    let mut completed = Vec::new();
    backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
    assert!(!backend.is_quiescent());

    backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
    let final_snapshot = backend.progress_snapshot();
    assert_eq!(final_snapshot.pending, 0);
    assert_eq!(final_snapshot.scheduled_events, 0);
    assert_eq!(final_snapshot.ready, 0);
    assert!(backend.is_quiescent());
  }

  #[test]
  fn dst_replays_same_cross_node_delivery_order_for_same_seed() {
    let first = dst_delivery_order_for_seed(41);
    let second = dst_delivery_order_for_seed(41);
    assert_eq!(first, second);
  }

  #[test]
  fn dst_different_seeds_change_connect_visibility_tick() {
    let ticks = [
      dst_connect_completion_tick(5),
      dst_connect_completion_tick(7),
      dst_connect_completion_tick(51),
    ];
    assert!(
      ticks[0] != ticks[1] || ticks[1] != ticks[2],
      "expected at least one seed to produce a different connect tick, got {ticks:?}"
    );
  }

  #[test]
  fn dst_preserves_stream_send_order_for_same_socket() {
    let payloads = [b"first".to_vec(), b"second".to_vec(), b"third".to_vec()];

    for seed in [1_u64, 5, 7, 19, 41, 51, 99] {
      let mut dst = DST::with_config(DSConfig {
        seed,
        max_delay_ticks: 2,
        fault_every: 0,
        network_faults: DSNetworkFaults::Off,
      });
      let mut sender = create_dst_backend(&mut dst);
      let receiver = create_dst_backend(&mut dst);

      dst_test_socket(&sender.node, 11);
      dst_test_socket(&receiver.node, 22);
      {
        let mut sender_node = sender.node.borrow_mut();
        sender_node.sockets.get_mut(&11).unwrap().peer =
          Some(DSTSocketRef { node_id: receiver.node_id, fd: 22 });
      }

      for (id, payload) in payloads.iter().enumerate() {
        let send_buf = crate::backend::op::MsgBuf::from_slice(payload);
        let send_msg = MsgSend::new(std::slice::from_ref(&send_buf), None);
        sender.network_complete_op(
          id as u64 + 1,
          Op::Send {
            fd: dst_resource(11),
            msg: send_msg,
            flags: SendFlags::EMPTY,
          },
        );
      }

      for _ in 0..16 {
        if !dst.step() {
          if dst.inner.borrow().events.is_empty() {
            break;
          }
          let _ = dst.tick();
        }
      }

      let inbox: Vec<u8> = receiver
        .node
        .borrow()
        .sockets
        .get(&22)
        .map(dst_stream_queue_bytes)
        .unwrap();
      let expected: Vec<u8> = payloads.iter().flatten().copied().collect();
      assert_eq!(inbox, expected, "seed {seed} reordered stream sends");
    }
  }

  #[test]
  fn dst_datagram_recv_preserves_packet_boundaries() {
    let mut dst = DST::with_config(DSConfig {
      seed: 7,
      max_delay_ticks: 1,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    let mut sender = create_dst_backend(&mut dst);
    let mut receiver = create_dst_backend(&mut dst);

    let mut sender_socket =
      DSTSocketHandle::new(SockDomain::IPV4, SockType::DGRAM, SockProto::UDP);
    sender_socket.state = DSTSocketState::Connected;
    sender_socket.peer =
      Some(DSTSocketRef { node_id: receiver.node_id, fd: 22 });
    sender.node.borrow_mut().sockets.insert(11, sender_socket);

    let mut receiver_socket =
      DSTSocketHandle::new(SockDomain::IPV4, SockType::DGRAM, SockProto::UDP);
    receiver_socket.state = DSTSocketState::Bound;
    receiver.node.borrow_mut().sockets.insert(22, receiver_socket);

    for (id, payload) in
      [b"alpha".as_slice(), b"beta".as_slice()].into_iter().enumerate()
    {
      let send_buf = crate::backend::op::MsgBuf::from_slice(payload);
      let send_msg = MsgSend::new(std::slice::from_ref(&send_buf), None);
      sender.network_complete_op(
        id as u64 + 1,
        Op::Send {
          fd: dst_resource(11),
          msg: send_msg,
          flags: SendFlags::EMPTY,
        },
      );
    }

    for _ in 0..8 {
      if !dst.step() {
        let _ = dst.tick();
      }
    }

    let mut buf = [0_u8; 16];
    let mut msg_buf = MsgBufMut::from_slice(&mut buf);
    let recv_msg = MsgRecv::new(std::slice::from_mut(&mut msg_buf));
    receiver.network_complete_op(
      10,
      Op::Recv { fd: dst_resource(22), msg: recv_msg, flags: RecvFlags::EMPTY },
    );
    let first = dst_take_ready(&receiver.node);
    assert_eq!(first.len(), 1);
    let first_len = first[0].result() as usize;
    assert_eq!(&buf[..first_len], b"alpha");

    let mut buf = [0_u8; 16];
    let mut msg_buf = MsgBufMut::from_slice(&mut buf);
    let recv_msg = MsgRecv::new(std::slice::from_mut(&mut msg_buf));
    receiver.network_complete_op(
      11,
      Op::Recv { fd: dst_resource(22), msg: recv_msg, flags: RecvFlags::EMPTY },
    );
    let second = dst_take_ready(&receiver.node);
    assert_eq!(second.len(), 1);
    let second_len = second[0].result() as usize;
    assert_eq!(&buf[..second_len], b"beta");
  }

  #[test]
  fn dst_stream_shutdown_write_yields_peer_eof() {
    let mut dst = DST::with_config(DSConfig {
      seed: 9,
      max_delay_ticks: 1,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    let mut sender = create_dst_backend(&mut dst);
    let mut receiver = create_dst_backend(&mut dst);

    let mut sender_socket =
      DSTSocketHandle::new(SockDomain::IPV4, SockType::STREAM, SockProto::TCP);
    sender_socket.state = DSTSocketState::Connected;
    sender_socket.peer =
      Some(DSTSocketRef { node_id: receiver.node_id, fd: 22 });
    sender.node.borrow_mut().sockets.insert(11, sender_socket);

    let mut receiver_socket =
      DSTSocketHandle::new(SockDomain::IPV4, SockType::STREAM, SockProto::TCP);
    receiver_socket.state = DSTSocketState::Connected;
    receiver_socket.peer =
      Some(DSTSocketRef { node_id: sender.node_id, fd: 11 });
    receiver.node.borrow_mut().sockets.insert(22, receiver_socket);

    let mut buf = [0_u8; 8];
    let mut msg_buf = MsgBufMut::from_slice(&mut buf);
    let recv_msg = MsgRecv::new(std::slice::from_mut(&mut msg_buf));
    receiver.network_complete_op(
      1,
      Op::Recv { fd: dst_resource(22), msg: recv_msg, flags: RecvFlags::EMPTY },
    );
    assert!(dst_take_ready(&receiver.node).is_empty());

    sender.network_complete_op(
      2,
      Op::Shutdown { fd: dst_resource(11), how: ShutdownHow::Write },
    );
    let sender_ready = dst_take_ready(&sender.node);
    assert_eq!(sender_ready.len(), 1);
    assert_eq!(sender_ready[0].result(), 0);

    for _ in 0..8 {
      if !dst.step() {
        let _ = dst.tick();
      }
    }

    let receiver_ready = dst_take_ready(&receiver.node);
    assert_eq!(receiver_ready.len(), 1);
    assert_eq!(receiver_ready[0].result(), 0);
  }

  #[test]
  fn dst_stream_reset_wakes_pending_recv_and_breaks_future_send() {
    let mut dst = DST::with_config(DSConfig {
      seed: 21,
      max_delay_ticks: 1,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    let mut sender = create_dst_backend(&mut dst);
    let mut receiver = create_dst_backend(&mut dst);

    let mut sender_socket =
      DSTSocketHandle::new(SockDomain::IPV4, SockType::STREAM, SockProto::TCP);
    sender_socket.state = DSTSocketState::Connected;
    sender_socket.peer =
      Some(DSTSocketRef { node_id: receiver.node_id, fd: 22 });
    sender.node.borrow_mut().sockets.insert(11, sender_socket);

    let mut receiver_socket =
      DSTSocketHandle::new(SockDomain::IPV4, SockType::STREAM, SockProto::TCP);
    receiver_socket.state = DSTSocketState::Connected;
    receiver_socket.peer =
      Some(DSTSocketRef { node_id: sender.node_id, fd: 11 });
    receiver.node.borrow_mut().sockets.insert(22, receiver_socket);

    let mut recv_buf = [0_u8; 8];
    let mut msg_buf = MsgBufMut::from_slice(&mut recv_buf);
    let recv_msg = MsgRecv::new(std::slice::from_mut(&mut msg_buf));
    receiver.network_complete_op(
      1,
      Op::Recv { fd: dst_resource(22), msg: recv_msg, flags: RecvFlags::EMPTY },
    );
    assert!(dst_take_ready(&receiver.node).is_empty());

    sender.schedule_socket_reset(
      DSTSocketRef { node_id: receiver.node_id, fd: 22 },
      libc::ECONNRESET,
    );

    for _ in 0..8 {
      if !dst.step() {
        let _ = dst.tick();
      }
    }

    let receiver_ready = dst_take_ready(&receiver.node);
    assert_eq!(receiver_ready.len(), 1);
    assert_eq!(receiver_ready[0].result(), -(libc::ECONNRESET as isize));

    let send_buf = crate::backend::op::MsgBuf::from_slice(b"hello");
    let send_msg = MsgSend::new(std::slice::from_ref(&send_buf), None);
    receiver.network_complete_op(
      2,
      Op::Send { fd: dst_resource(22), msg: send_msg, flags: SendFlags::EMPTY },
    );
    let receiver_ready = dst_take_ready(&receiver.node);
    assert_eq!(receiver_ready.len(), 1);
    assert_eq!(receiver_ready[0].result(), -(libc::ECONNRESET as isize));
  }

  #[test]
  fn dst_listener_backlog_limits_pending_accepts() {
    let mut dst = DST::with_config(DSConfig {
      seed: 13,
      max_delay_ticks: 1,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    let mut listener_backend = create_dst_backend(&mut dst);
    let mut client_a = create_dst_backend(&mut dst);
    let mut client_b = create_dst_backend(&mut dst);

    listener_backend.network_complete_op(
      1,
      Op::Socket {
        domain: SockDomain::IPV4,
        ty: SockType::STREAM,
        proto: SockProto::TCP,
      },
    );
    let listener_fd = dst_take_ready(&listener_backend.node)[0].result() as i32;
    let listener = dst_resource(listener_fd);
    let addr: SocketAddr = "127.0.0.1:7010".parse().unwrap();
    listener_backend
      .network_complete_op(2, Op::Bind { fd: listener.clone(), addr });
    assert_eq!(dst_take_ready(&listener_backend.node)[0].result(), 0);
    listener_backend
      .network_complete_op(3, Op::Listen { fd: listener.clone(), backlog: 1 });
    assert_eq!(dst_take_ready(&listener_backend.node)[0].result(), 0);

    for backend in [&mut client_a, &mut client_b] {
      backend.network_complete_op(
        10,
        Op::Socket {
          domain: SockDomain::IPV4,
          ty: SockType::STREAM,
          proto: SockProto::TCP,
        },
      );
    }
    let client_a_fd = dst_take_ready(&client_a.node)[0].result() as i32;
    let client_b_fd = dst_take_ready(&client_b.node)[0].result() as i32;

    client_a.network_complete_op(
      11,
      Op::Connect {
        fd: dst_resource(client_a_fd),
        addr: op::socket_addr_into_buf(addr),
      },
    );
    assert!(dst_take_ready(&client_a.node).is_empty());

    client_b.network_complete_op(
      12,
      Op::Connect {
        fd: dst_resource(client_b_fd),
        addr: op::socket_addr_into_buf(addr),
      },
    );
    let client_b_ready = dst_take_ready(&client_b.node);
    assert_eq!(client_b_ready.len(), 1);
    assert_eq!(client_b_ready[0].result(), -(libc::ECONNREFUSED as isize));

    for _ in 0..8 {
      if !dst.step() {
        let _ = dst.tick();
      }
    }

    let client_a_ready = dst_take_ready(&client_a.node);
    assert_eq!(client_a_ready.len(), 1);
    assert_eq!(client_a_ready[0].result(), 0);

    listener_backend.network_complete_op(
      20,
      Op::Accept {
        fd: listener,
        addr: NonNull::new(Box::leak(Box::new(SocketAddrBuf::unspecified())))
          .unwrap(),
      },
    );
    let listener_ready = dst_take_ready(&listener_backend.node);
    assert_eq!(listener_ready.len(), 1);
    assert!(listener_ready[0].result() >= DST_SOCKET_RESOURCE_BASE as isize);
  }

  #[test]
  fn dst_rejects_invalid_socket_state_transitions() {
    let mut dst = DST::with_config(DSConfig {
      seed: 17,
      max_delay_ticks: 1,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    let mut backend = create_dst_backend(&mut dst);

    backend.network_complete_op(
      1,
      Op::Socket {
        domain: SockDomain::IPV4,
        ty: SockType::STREAM,
        proto: SockProto::TCP,
      },
    );
    let stream_fd = dst_take_ready(&backend.node)[0].result() as i32;
    let stream = dst_resource(stream_fd);

    backend
      .network_complete_op(2, Op::Listen { fd: stream.clone(), backlog: 1 });
    let ready = dst_take_ready(&backend.node);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].result(), -(libc::EINVAL as isize));

    let send_buf = crate::backend::op::MsgBuf::from_slice(b"hello");
    let send_msg = MsgSend::new(std::slice::from_ref(&send_buf), None);
    backend.network_complete_op(
      3,
      Op::Send { fd: stream.clone(), msg: send_msg, flags: SendFlags::EMPTY },
    );
    let ready = dst_take_ready(&backend.node);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].result(), -(libc::ENOTCONN as isize));

    backend.network_complete_op(
      4,
      Op::Socket {
        domain: SockDomain::IPV4,
        ty: SockType::DGRAM,
        proto: SockProto::UDP,
      },
    );
    let dgram_fd = dst_take_ready(&backend.node)[0].result() as i32;
    let dgram = dst_resource(dgram_fd);

    backend
      .network_complete_op(5, Op::Listen { fd: dgram.clone(), backlog: 1 });
    let ready = dst_take_ready(&backend.node);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].result(), -(libc::EOPNOTSUPP as isize));

    let mut recv_buf = [0_u8; 8];
    let mut msg_buf = MsgBufMut::from_slice(&mut recv_buf);
    let recv_msg = MsgRecv::new(std::slice::from_mut(&mut msg_buf));
    backend.network_complete_op(
      6,
      Op::Accept {
        fd: dgram,
        addr: NonNull::new(Box::leak(Box::new(SocketAddrBuf::unspecified())))
          .unwrap(),
      },
    );
    let ready = dst_take_ready(&backend.node);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].result(), -(libc::EINVAL as isize));

    backend.network_complete_op(
      7,
      Op::Recv { fd: stream, msg: recv_msg, flags: RecvFlags::EMPTY },
    );
    let ready = dst_take_ready(&backend.node);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].result(), -(libc::ENOTCONN as isize));
  }

  #[test]
  fn dst_preserves_intra_node_event_order_while_reordering_across_nodes() {
    for seed in [1_u64, 7, 19, 41, 99] {
      let mut dst = DST::with_config(DSConfig {
        seed,
        max_delay_ticks: 0,
        fault_every: 0,
        network_faults: DSNetworkFaults::Off,
      });
      let sender_a = create_dst_backend(&mut dst);
      let sender_b = create_dst_backend(&mut dst);
      let receiver = create_dst_backend(&mut dst);

      dst_test_socket(&receiver.node, 91);

      {
        let mut inner = dst.inner.borrow_mut();
        inner.enqueue_event(DSTEvent::StreamDelivered {
          ready_at_tick: 0,
          source_node: sender_a.node_id,
          source_seq: 0,
          from: DSTSocketRef { node_id: sender_a.node_id, fd: 11 },
          to: DSTSocketRef { node_id: receiver.node_id, fd: 91 },
          bytes: b"1".to_vec(),
        });
        inner.enqueue_event(DSTEvent::StreamDelivered {
          ready_at_tick: 0,
          source_node: sender_b.node_id,
          source_seq: 0,
          from: DSTSocketRef { node_id: sender_b.node_id, fd: 22 },
          to: DSTSocketRef { node_id: receiver.node_id, fd: 91 },
          bytes: b"x".to_vec(),
        });
        inner.enqueue_event(DSTEvent::StreamDelivered {
          ready_at_tick: 0,
          source_node: sender_a.node_id,
          source_seq: 1,
          from: DSTSocketRef { node_id: sender_a.node_id, fd: 11 },
          to: DSTSocketRef { node_id: receiver.node_id, fd: 91 },
          bytes: b"2".to_vec(),
        });
      }

      assert!(dst.step());
      let inbox: Vec<u8> = receiver
        .node
        .borrow()
        .sockets
        .get(&91)
        .map(dst_stream_queue_bytes)
        .unwrap();
      let pos1 = inbox.iter().position(|&byte| byte == b'1').unwrap();
      let pos2 = inbox.iter().position(|&byte| byte == b'2').unwrap();
      assert!(pos1 < pos2, "seed {seed} reordered same-node events: {inbox:?}");
    }
  }

  #[test]
  fn openat_then_read_uses_simulated_file_state() {
    let mut backend = DSBackend::with_config(DSConfig {
      seed: 1,
      max_delay_ticks: 0,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    backend.init(8).unwrap();
    backend.world.fs.insert(
      b"/tmp/demo".to_vec(),
      SimNode::File { bytes: b"demo-bytes".to_vec(), mode: 0o644 },
    );

    let path = std::ffi::OsString::from("/tmp/demo");
    let mut step_bump = Bump::new();
    backend.push(
      1,
      Op::OpenAt {
        dir_fd: Resource::cwd(),
        path,
        flags: crate::backend::op::OpenFlags::from_bits(libc::O_RDONLY),
        mode: crate::backend::op::FileMode::from_bits(0),
      },
      &mut step_bump,
    );
    backend.flush().unwrap();

    let mut completed = Vec::new();
    backend.wait(None, &mut completed).unwrap();
    assert_eq!(completed.len(), 1);
    assert!(completed[0].result() >= 0);
  }

  #[test]
  fn ds_fsync_succeeds_for_simulated_files_and_rejects_sockets() {
    let mut backend = DSBackend::with_config(DSConfig {
      seed: 1,
      max_delay_ticks: 0,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });

    let file_fd = 1_000_000;
    backend.world.resources.insert(
      file_fd,
      SimResource::File(
        backend.make_file_handle(b"/tmp/file".to_vec(), libc::O_RDWR),
      ),
    );
    let file_resource = dst_resource(file_fd);
    let file_result = backend.complete_op(Op::Fsync { fd: file_resource });
    assert!(matches!(file_result, OpAction::Complete(0)));

    let socket_fd = 1_000_001;
    backend.world.resources.insert(
      socket_fd,
      SimResource::Socket(SimSocketHandle {
        domain: SockDomain::IPV4,
        ty: SockType::STREAM,
        proto: SockProto::TCP,
        inbox: VecDeque::new(),
        bound: None,
        peer_addr: None,
        listening: false,
        backlog: 0,
        shutdown_read: false,
        shutdown_write: false,
        pending_accept: VecDeque::new(),
        peer: None,
      }),
    );
    let socket_resource = dst_resource(socket_fd);
    let socket_result = backend.complete_op(Op::Fsync { fd: socket_resource });
    assert!(matches!(
      socket_result,
      OpAction::Complete(result) if result == -(libc::EINVAL as isize)
    ));
  }

  #[test]
  fn dst_fsync_rejects_simulated_network_sockets() {
    let mut dst = DST::with_config(DSConfig {
      seed: 7,
      max_delay_ticks: 0,
      fault_every: 0,
      network_faults: DSNetworkFaults::Off,
    });
    let mut backend = create_dst_backend(&mut dst);
    backend.init(8).unwrap();
    dst_test_socket(&backend.node, 91);

    let fd = dst_resource(91);
    backend.push(1, Op::Fsync { fd }, &mut Bump::new());
    backend.flush().unwrap();
    assert!(dst.step());

    let mut completed = Vec::new();
    backend.wait(Some(Duration::ZERO), &mut completed).unwrap();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].result(), -(libc::EINVAL as isize));
  }
}
