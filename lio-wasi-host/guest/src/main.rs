//! WASI 0.3 workloads, timed inside the guest.
//!
//! `bench-guest WORKLOAD [N]` runs one and prints `WORKLOAD <milliseconds>
//! <what was done>`. It expects a preopened directory holding `big.bin`,
//! `small.txt` and a directory `many`, which `bench.sh` makes.

use std::time::Instant;

use lio_wasi::fs::{
  Descriptor, DescriptorFlags, DirEntryBuf, OpenAt, OpenFlags, PathFlags,
  StatAt, StreamStatus, UnlinkFileAt,
};
use lio_wasi::sockets::{
  Accepted, Connect, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress,
  Listener, TcpSocket,
};
use lio_wasi::{
  Call, Event, ReadStream, ReadableFuture, Started, StreamResult, WaitableSet,
  WriteStream,
};

const CHUNK: usize = 64 * 1024;

fn run<C: Call>(set: &mut WaitableSet, call: &mut C) {
  // SAFETY: `call` is borrowed for this whole function, which waits for
  // the call to return.
  if let Some(subtask) = unsafe { set.start(call) } {
    assert_eq!(set.wait(), Some(Event::Returned(subtask)));
  }
}

fn read_status<F: ReadableFuture>(set: &mut WaitableSet, status: &mut F) {
  // SAFETY: `status` is borrowed for this whole function, which waits for
  // the value to be delivered.
  if let Some(future) = unsafe { set.read_future(status) } {
    assert_eq!(set.wait(), Some(Event::FutureRead(future)));
  }
}

fn read(
  set: &mut WaitableSet,
  stream: ReadStream,
  buf: &mut [u8],
) -> StreamResult {
  // SAFETY: `buf` is borrowed for this whole function, which waits for the
  // read to end.
  match unsafe { set.read(stream, buf.as_mut_ptr(), buf.len()) } {
    Ok(Started::Done(result)) => result,
    Ok(Started::Blocked) => match set.wait() {
      Some(Event::Read(ended, result)) if ended == stream => result,
      other => unreachable!("only the read is in flight: {other:?}"),
    },
    Err(error) => panic!("the read was refused: {error}"),
  }
}

fn write(
  set: &mut WaitableSet,
  stream: WriteStream,
  data: &[u8],
) -> StreamResult {
  // SAFETY: `data` is borrowed for this whole function, which waits for
  // the write to end.
  match unsafe { set.write(stream, data.as_ptr(), data.len()) } {
    Ok(Started::Done(result)) => result,
    Ok(Started::Blocked) => match set.wait() {
      Some(Event::Write(ended, result)) if ended == stream => result,
      other => unreachable!("only the write is in flight: {other:?}"),
    },
    Err(error) => panic!("the write was refused: {error}"),
  }
}

fn open(
  set: &mut WaitableSet,
  dir: &Descriptor,
  path: &str,
  how: OpenFlags,
  flags: DescriptorFlags,
) -> Descriptor {
  let mut call = OpenAt::new(dir, PathFlags::SYMLINK_FOLLOW, path, how, flags);
  run(set, &mut call);
  match call.result() {
    Some(Ok(descriptor)) => descriptor,
    other => {
      panic!("opening {path} failed: {:?}", other.map(|r| r.map(|_| ())))
    }
  }
}

/// One stream over the whole of `big.bin`, read a chunk at a time.
fn read_file(set: &mut WaitableSet, dir: &Descriptor) -> String {
  let file =
    open(set, dir, "big.bin", OpenFlags::empty(), DescriptorFlags::READ);
  let (stream, _status) = file.read_via_stream(0);
  let stream = set.add_reader(stream);
  let mut buf = vec![0u8; CHUNK];
  let (mut total, mut reads) = (0usize, 0usize);
  loop {
    let done = read(set, stream, &mut buf);
    total += done.count();
    reads += 1;
    if let StreamResult::Dropped(_) = done {
      break;
    }
  }
  assert_eq!(set.close_reader(stream), Ok(None));
  format!("{total} bytes in {reads} reads")
}

/// One stream into `out.bin`, written `chunk` bytes at a time.
fn write_file(
  set: &mut WaitableSet,
  dir: &Descriptor,
  total: usize,
  chunk: usize,
) -> String {
  let how = OpenFlags::CREATE | OpenFlags::TRUNCATE;
  let file = open(set, dir, "out.bin", how, DescriptorFlags::WRITE);
  let (writer, reader) = lio_wasi::stream();
  let mut status = StreamStatus::new(file.write_via_stream(reader, 0));
  let writer = set.add_writer(writer);
  let data = vec![0x5au8; chunk];
  let (mut sent, mut writes) = (0usize, 0usize);
  while sent < total {
    let wrote = write(set, writer, &data[..chunk.min(total - sent)]);
    assert!(
      matches!(wrote, StreamResult::Completed(_)),
      "write ended: {wrote:?}"
    );
    sent += wrote.count();
    writes += 1;
  }
  assert_eq!(set.close_writer(writer), Ok(None));
  read_status(set, &mut status);
  assert!(matches!(status.result(), Some(Ok(()))));
  format!("{sent} bytes in {writes} writes")
}

fn stat(set: &mut WaitableSet, dir: &Descriptor, count: usize) -> String {
  for _ in 0..count {
    let mut call = StatAt::new(dir, PathFlags::empty(), "small.txt");
    run(set, &mut call);
    assert!(matches!(call.result(), Some(Ok(_))));
  }
  format!("{count} stat-at calls")
}

fn open_close(set: &mut WaitableSet, dir: &Descriptor, count: usize) -> String {
  for _ in 0..count {
    drop(open(
      set,
      dir,
      "small.txt",
      OpenFlags::empty(),
      DescriptorFlags::READ,
    ));
  }
  format!("{count} open-at calls")
}

/// Opens without following a final symbolic link, which a host has to check
/// for.
fn open_nofollow(
  set: &mut WaitableSet,
  dir: &Descriptor,
  count: usize,
) -> String {
  for _ in 0..count {
    let mut call = OpenAt::new(
      dir,
      PathFlags::empty(),
      "small.txt",
      OpenFlags::empty(),
      DescriptorFlags::READ,
    );
    run(set, &mut call);
    assert!(matches!(call.result(), Some(Ok(_))));
  }
  format!("{count} open-at calls, not following links")
}

/// Open, read through a stream of its own, close: what one positional read
/// costs a guest that makes a stream per operation, as lio's backend does.
fn small_reads(
  set: &mut WaitableSet,
  dir: &Descriptor,
  count: usize,
) -> String {
  let file =
    open(set, dir, "small.txt", OpenFlags::empty(), DescriptorFlags::READ);
  let mut buf = vec![0u8; 4096];
  let mut total = 0usize;
  for _ in 0..count {
    let (stream, _status) = file.read_via_stream(0);
    let stream = set.add_reader(stream);
    total += read(set, stream, &mut buf).count();
    let _ = set.close_reader(stream);
  }
  format!("{count} streams, {total} bytes")
}

fn list(set: &mut WaitableSet, dir: &Descriptor) -> String {
  let how = OpenFlags::DIRECTORY;
  let many = open(set, dir, "many", how, DescriptorFlags::READ);
  let (entries, _status) = many.read_directory();
  let entries = set.add_dir_reader(entries);
  let mut buf = DirEntryBuf::with_capacity(64);
  let (mut found, mut reads) = (0usize, 0usize);
  loop {
    // SAFETY: `buf` outlives the read, which has ended once this statement
    // has.
    let done = match unsafe { set.read_dir(entries, &mut buf) } {
      Ok(Started::Done(result)) => result,
      Ok(Started::Blocked) => match set.wait() {
        Some(Event::ReadDir(ended, result)) if ended == entries => result,
        other => unreachable!("only the listing is in flight: {other:?}"),
      },
      Err(error) => panic!("the listing read was refused: {error}"),
    };
    reads += 1;
    found += buf.drain().count();
    if let StreamResult::Dropped(_) = done {
      break;
    }
  }
  let _ = set.close_dir_reader(entries);
  format!("{found} entries in {reads} reads")
}

fn loopback(port: u16) -> IpSocketAddress {
  IpSocketAddress::Ipv4(Ipv4SocketAddress { port, address: (127, 0, 0, 1) })
}

/// A listening socket on a loopback port of the system's choosing.
fn listen(set: &mut WaitableSet) -> (TcpSocket, Listener, u16) {
  let listener = TcpSocket::create(IpAddressFamily::Ipv4).expect("create");
  listener.bind(loopback(0)).expect("bind");
  let connections = listener.listen().expect("listen");
  let port = match listener.get_local_address().expect("local address") {
    IpSocketAddress::Ipv4(v4) => v4.port,
    IpSocketAddress::Ipv6(v6) => v6.port,
  };
  (listener, set.add_listener(connections), port)
}

/// Connects to `port` and accepts that connection: `(client, server)`.
fn connect(
  set: &mut WaitableSet,
  connections: Listener,
  port: u16,
) -> (TcpSocket, TcpSocket) {
  let client = TcpSocket::create(IpAddressFamily::Ipv4).expect("create");
  let mut accepted = Accepted::new();
  let mut connect = Connect::new(&client, loopback(port));
  // SAFETY: `accepted` and `connect` stay put until both have ended, which
  // the loop below waits for.
  let (accept, _connecting) = unsafe {
    (set.accept(connections, &mut accepted), set.start(&mut connect))
  };
  assert!(accept.is_ok());
  while set.wait().is_some() {}
  assert!(matches!(connect.result(), Some(Ok(()))));
  drop(connect);
  let server = accepted.socket().expect("an accepted connection");
  (client, server)
}

/// Sends `total` bytes over a loopback TCP connection whose two ends are
/// both in this component.
fn tcp(set: &mut WaitableSet, total: usize) -> String {
  let (_listener, connections, port) = listen(set);
  let (client, server) = connect(set, connections, port);

  let (input, _input_status) = server.receive();
  let input = set.add_reader(input);
  let (writer, reader) = lio_wasi::stream();
  let _output_status = client.send(reader);
  let output = set.add_writer(writer);

  let data = vec![0xa5u8; CHUNK];
  let mut buf = vec![0u8; CHUNK];
  let (mut sent, mut received) = (0usize, 0usize);
  let (mut writing, mut reading, mut open) = (false, false, true);
  loop {
    if open && !writing {
      if sent >= total {
        let _ = set.close_writer(output);
        open = false;
      } else {
        let len = CHUNK.min(total - sent);
        // SAFETY: `data` is not touched until the write has ended.
        match unsafe { set.write(output, data.as_ptr(), len) } {
          Ok(Started::Done(wrote)) => {
            sent += wrote.count();
            continue;
          }
          Ok(Started::Blocked) => writing = true,
          Err(error) => panic!("the write was refused: {error}"),
        }
      }
    }
    if !reading {
      // SAFETY: `buf` is not touched until the read has ended.
      match unsafe { set.read(input, buf.as_mut_ptr(), buf.len()) } {
        Ok(Started::Done(read)) => {
          received += read.count();
          if let StreamResult::Dropped(_) = read {
            break;
          }
          continue;
        }
        Ok(Started::Blocked) => reading = true,
        Err(error) => panic!("the read was refused: {error}"),
      }
    }
    match set.wait() {
      Some(Event::Write(_, wrote)) => {
        writing = false;
        sent += wrote.count();
      }
      Some(Event::Read(_, read)) => {
        reading = false;
        received += read.count();
        if let StreamResult::Dropped(_) = read {
          break;
        }
      }
      other => unreachable!("an event for nothing started here: {other:?}"),
    }
  }
  assert_eq!(received, total);
  let _ = set.close_reader(input);
  let _ = set.close_listener(connections);
  format!("{received} bytes")
}

/// Opens `count` files, 64 at a time: each round starts its calls together
/// and then waits for them all.
fn burst(set: &mut WaitableSet, dir: &Descriptor, count: usize) -> String {
  const AT_ONCE: usize = 64;
  let rounds = count / AT_ONCE;
  for _ in 0..rounds {
    let mut calls: Vec<OpenAt<'_>> = (0..AT_ONCE)
      .map(|_| {
        OpenAt::new(
          dir,
          PathFlags::SYMLINK_FOLLOW,
          "small.txt",
          OpenFlags::empty(),
          DescriptorFlags::READ,
        )
      })
      .collect();
    for call in &mut calls {
      // SAFETY: `calls` is not touched again until every call has
      // returned, which the wait below sees.
      let _ = unsafe { set.start(call) };
    }
    while set.wait().is_some() {}
    for call in &mut calls {
      assert!(matches!(call.result(), Some(Ok(_))));
    }
  }
  format!("{} open-at calls, {AT_ONCE} at a time", rounds * AT_ONCE)
}

/// Sends a datagram to each of 64 sockets whose receives are already
/// waiting, and takes them all in: every receive has to wait.
fn fan(set: &mut WaitableSet, rounds: usize) -> String {
  use lio_wasi::sockets::{ReceiveFrom, SendTo, UdpSocket};
  const AT_ONCE: usize = 64;
  let sockets: Vec<UdpSocket> = (0..AT_ONCE)
    .map(|_| {
      let socket = UdpSocket::create(IpAddressFamily::Ipv4).expect("create");
      socket.bind(loopback(0)).expect("bind");
      socket
    })
    .collect();
  let addresses: Vec<IpSocketAddress> = sockets
    .iter()
    .map(|socket| socket.get_local_address().expect("local address"))
    .collect();
  let sender = UdpSocket::create(IpAddressFamily::Ipv4).expect("create");
  sender.bind(loopback(0)).expect("bind");
  let payload = [0x42u8; 64];
  let mut received = 0usize;
  for _ in 0..rounds {
    let mut receives: Vec<ReceiveFrom<'_>> =
      sockets.iter().map(ReceiveFrom::new).collect();
    let mut sends: Vec<SendTo<'_>> = addresses
      .iter()
      .map(|address| SendTo::new(&sender, &payload, Some(*address)))
      .collect();
    // SAFETY: neither vector is touched again until every call has
    // returned, which the wait below sees.
    unsafe {
      for receive in &mut receives {
        let _ = set.start(receive);
      }
      for send in &mut sends {
        let _ = set.start(send);
      }
    }
    while set.wait().is_some() {}
    for receive in &mut receives {
      assert!(matches!(receive.result(), Some(Ok(_))));
      received += 1;
    }
  }
  format!("{received} datagrams, {AT_ONCE} receives waiting at a time")
}

/// Hands standard input's stream straight to a file: the bytes go from one
/// host function to another and never enter this component.
fn splice(set: &mut WaitableSet, dir: &Descriptor) -> String {
  let how = OpenFlags::CREATE | OpenFlags::TRUNCATE;
  let file = open(set, dir, "spliced.bin", how, DescriptorFlags::WRITE);
  let (input, _input_status) = lio_wasi::cli::stdin::read_via_stream();
  let mut status = StreamStatus::new(file.write_via_stream(input, 0));
  read_status(set, &mut status);
  assert!(matches!(status.result(), Some(Ok(()))));
  String::from("standard input to spliced.bin")
}

/// Hands `big.bin`'s stream straight to `copy.bin`: file to file inside the
/// host, with nothing entering this component.
fn copy(set: &mut WaitableSet, dir: &Descriptor) -> String {
  let from =
    open(set, dir, "big.bin", OpenFlags::empty(), DescriptorFlags::READ);
  let how = OpenFlags::CREATE | OpenFlags::TRUNCATE;
  let to = open(set, dir, "copy.bin", how, DescriptorFlags::WRITE);
  let (data, _read_status) = from.read_via_stream(0);
  let mut status = StreamStatus::new(to.write_via_stream(data, 0));
  read_status(set, &mut status);
  assert!(matches!(status.result(), Some(Ok(()))));
  String::from("big.bin to copy.bin")
}

/// Reads until `buf` is full.
fn read_all(set: &mut WaitableSet, stream: ReadStream, buf: &mut [u8]) {
  let mut filled = 0;
  while filled < buf.len() {
    let done = read(set, stream, &mut buf[filled..]);
    assert!(matches!(done, StreamResult::Completed(_)), "ended: {done:?}");
    filled += done.count();
  }
}

/// A 64-byte request and a 64-byte reply, `count` times over one loopback
/// connection: every read waits for the other side.
fn echo(set: &mut WaitableSet, count: usize) -> String {
  const MESSAGE: usize = 64;
  let (_listener, connections, port) = listen(set);
  let (client, server) = connect(set, connections, port);

  let (requests, _) = server.receive();
  let requests = set.add_reader(requests);
  let (replies, _) = client.receive();
  let replies = set.add_reader(replies);
  let (writer, reader) = lio_wasi::stream();
  let _requesting = client.send(reader);
  let request = set.add_writer(writer);
  let (writer, reader) = lio_wasi::stream();
  let _replying = server.send(reader);
  let reply = set.add_writer(writer);

  let message = [0x3cu8; MESSAGE];
  let mut buf = [0u8; MESSAGE];
  for _ in 0..count {
    assert_eq!(write(set, request, &message).count(), MESSAGE);
    read_all(set, requests, &mut buf);
    assert_eq!(write(set, reply, &buf).count(), MESSAGE);
    read_all(set, replies, &mut buf);
  }
  let _ = set.close_writer(request);
  let _ = set.close_writer(reply);
  let _ = set.close_reader(requests);
  let _ = set.close_reader(replies);
  let _ = set.close_listener(connections);
  format!("{count} round trips of {MESSAGE} bytes")
}

/// Makes `count` loopback connections one after another, dropping both ends
/// of each: what a connection costs to set up and tear down.
fn conns(set: &mut WaitableSet, count: usize) -> String {
  let (_listener, connections, port) = listen(set);
  for _ in 0..count {
    drop(connect(set, connections, port));
  }
  let _ = set.close_listener(connections);
  format!("{count} connections")
}

/// Create a file, write 4 KiB to it, close it, stat it and remove it,
/// `count` times: a temporary file's whole life.
fn churn(set: &mut WaitableSet, dir: &Descriptor, count: usize) -> String {
  let data = vec![0x7eu8; 4096];
  for _ in 0..count {
    let how = OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let file = open(set, dir, "churn.tmp", how, DescriptorFlags::WRITE);
    let (writer, reader) = lio_wasi::stream();
    let mut status = StreamStatus::new(file.write_via_stream(reader, 0));
    let writer = set.add_writer(writer);
    assert_eq!(write(set, writer, &data).count(), data.len());
    assert_eq!(set.close_writer(writer), Ok(None));
    read_status(set, &mut status);
    assert!(matches!(status.result(), Some(Ok(()))));
    drop(file);
    let mut stat = StatAt::new(dir, PathFlags::empty(), "churn.tmp");
    run(set, &mut stat);
    assert!(matches!(stat.result(), Some(Ok(_))));
    let mut unlink = UnlinkFileAt::new(dir, "churn.tmp");
    run(set, &mut unlink);
    assert!(matches!(unlink.result(), Some(Ok(()))));
  }
  format!("{count} files made, written, checked and removed")
}

fn main() {
  let arguments = lio_wasi::cli::environment::get_arguments();
  let workload = arguments.get(1).map_or("", String::as_str).to_owned();
  let count: usize =
    arguments.get(2).and_then(|n| n.parse().ok()).unwrap_or(10_000);
  let mut set = WaitableSet::new();
  let dir = lio_wasi::fs::preopens::get_directories().into_iter().next();
  let dir = dir.map(|(dir, _name)| dir);
  let dir = || dir.as_ref().expect("a preopened directory");

  let started = Instant::now();
  let did = match workload.as_str() {
    "read" => read_file(&mut set, dir()),
    "write" => write_file(&mut set, dir(), count, CHUNK),
    "smallw" => write_file(&mut set, dir(), count * 4096, 4096),
    "copy" => copy(&mut set, dir()),
    "echo" => echo(&mut set, count),
    "conns" => conns(&mut set, count),
    "churn" => churn(&mut set, dir(), count),
    "stat" => stat(&mut set, dir(), count),
    "open" => open_close(&mut set, dir(), count),
    "nofollow" => open_nofollow(&mut set, dir(), count),
    "small" => small_reads(&mut set, dir(), count),
    "list" => list(&mut set, dir()),
    "tcp" => tcp(&mut set, count),
    "splice" => splice(&mut set, dir()),
    "burst" => burst(&mut set, dir(), count),
    "fan" => fan(&mut set, count),
    other => panic!("unknown workload {other:?}"),
  };
  let elapsed = started.elapsed();
  println!("{workload} {:.3} {did}", elapsed.as_secs_f64() * 1e3);
}
