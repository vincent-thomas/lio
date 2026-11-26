//! WASI 0.3 backend, for WebAssembly components.
//!
//! Built on [`lio_wasi::Queue`], which is completion-based like io_uring:
//! an operation is started with caller memory and reports once it is done.
//!
//! # Resources
//!
//! A WASI resource is a typed handle, not a descriptor number, and one lio
//! resource needs more than a handle (a file also has a cursor). So a
//! [`Resource`] made by this backend holds an id into a per-thread table of
//! what it stands for. The table is thread-local because closing a resource
//! is a plain function with no backend to reach through; a component has one
//! thread, so nothing is shared.
//!
//! Ids `0`, `1` and `2` are the component's standard streams, and
//! [`Resource::cwd`](crate::api::resource::Resource) resolves against the
//! preopened directories.
//!
//! # What is supported
//!
//! Files, directories-as-anchors, standard streams, timeouts and
//! cancellation. Sockets, directory listing and process spawning complete
//! with `ENOTSUP` for now.

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io;
use std::os::fd::AsRawFd;
use std::time::Duration;

use bumpalo::Bump;
use lio_wasi::clock::WaitFor;
use lio_wasi::fs::{
  self, Descriptor, DescriptorFlags, DescriptorStat, DescriptorType, ErrorCode,
  PathFlags,
};
use lio_wasi::wasip3;
use lio_wasi::{Completion, Outcome, Queue, ReadStream, WriteStream};

use crate::api::resource::{RawResource, ResourceClose};
use crate::backend::native;
use crate::backend::op::{
  ByteSpan, FileStat, FileType, LinkKind, Op, OpenFlags, StatTarget,
  UnlinkKind, WriteFlags, is_valid_file_offset,
};
use crate::backend::{InFlight, IoBackend, Keep, OpCompleted};

/// `user_data` of the timer that bounds a [`wait`](IoBackend::wait). No
/// registration id is this value.
const TIMER: u64 = u64::MAX;

/// The first id this backend gives a resource. Ids below it are the
/// standard streams and whatever descriptors wasi-libc hands out, which this
/// backend does not own.
const FIRST_ID: RawResource = 1 << 16;

fn fail(errno: i32) -> isize {
  -(errno as isize)
}

const CANCELLED: isize = -(libc::ECANCELED as isize);

/// The errno closest to a WASI filesystem error.
fn errno(error: &ErrorCode) -> i32 {
  match error {
    ErrorCode::Access => libc::EACCES,
    ErrorCode::Already => libc::EALREADY,
    ErrorCode::BadDescriptor => libc::EBADF,
    ErrorCode::Busy => libc::EBUSY,
    ErrorCode::Deadlock => libc::EDEADLK,
    ErrorCode::Quota => libc::EDQUOT,
    ErrorCode::Exist => libc::EEXIST,
    ErrorCode::FileTooLarge => libc::EFBIG,
    ErrorCode::IllegalByteSequence => libc::EILSEQ,
    ErrorCode::InProgress => libc::EINPROGRESS,
    ErrorCode::Interrupted => libc::EINTR,
    ErrorCode::Invalid => libc::EINVAL,
    ErrorCode::Io => libc::EIO,
    ErrorCode::IsDirectory => libc::EISDIR,
    ErrorCode::Loop => libc::ELOOP,
    ErrorCode::TooManyLinks => libc::EMLINK,
    ErrorCode::MessageSize => libc::EMSGSIZE,
    ErrorCode::NameTooLong => libc::ENAMETOOLONG,
    ErrorCode::NoDevice => libc::ENODEV,
    ErrorCode::NoEntry => libc::ENOENT,
    ErrorCode::NoLock => libc::ENOLCK,
    ErrorCode::InsufficientMemory => libc::ENOMEM,
    ErrorCode::InsufficientSpace => libc::ENOSPC,
    ErrorCode::NotDirectory => libc::ENOTDIR,
    ErrorCode::NotEmpty => libc::ENOTEMPTY,
    ErrorCode::NotRecoverable => libc::ENOTRECOVERABLE,
    ErrorCode::Unsupported => libc::ENOTSUP,
    ErrorCode::NoTty => libc::ENOTTY,
    ErrorCode::NoSuchDevice => libc::ENXIO,
    ErrorCode::Overflow => libc::EOVERFLOW,
    ErrorCode::NotPermitted => libc::EPERM,
    ErrorCode::Pipe => libc::EPIPE,
    ErrorCode::ReadOnly => libc::EROFS,
    ErrorCode::InvalidSeek => libc::ESPIPE,
    ErrorCode::TextFileBusy => libc::ETXTBSY,
    ErrorCode::CrossDevice => libc::EXDEV,
    ErrorCode::Other(_) => libc::EIO,
  }
}

// ───────────────────────────── resource table ───────────────────────────────

/// An open file or directory.
struct File {
  descriptor: Descriptor,
  /// Where the next read or write without an offset happens. WASI files
  /// have no position of their own.
  cursor: u64,
  /// Opened for appending: writes without an offset go to the end.
  append: bool,
}

/// What this thread's resource ids stand for.
#[derive(Default)]
struct Table {
  files: Vec<Option<File>>,
  free: Vec<usize>,
  /// The preopened directories and their names, fetched on first use.
  preopens: Option<Vec<(Descriptor, String)>>,
}

thread_local! {
  static TABLE: RefCell<Table> = RefCell::new(Table::default());
  /// Resources closed while the table was in use. The backend closes them
  /// the next time it runs.
  static DEFERRED: RefCell<Vec<RawResource>> = const { RefCell::new(Vec::new()) };
}

fn with_table<R>(run: impl FnOnce(&mut Table) -> R) -> R {
  TABLE.with(|table| run(&mut table.borrow_mut()))
}

/// Closes a resource this backend made. This is its [`ResourceClose`].
fn close_resource(raw: RawResource) {
  let closed = TABLE.with(|table| match table.try_borrow_mut() {
    Ok(mut table) => Some(table.remove(raw)),
    Err(_) => None,
  });
  match closed {
    // Dropped here, after the table borrow ended.
    Some(file) => drop(file),
    None => DEFERRED.with(|deferred| deferred.borrow_mut().push(raw)),
  }
}

fn close_deferred() {
  let pending = DEFERRED.with(|deferred| deferred.take());
  for raw in pending {
    let file = with_table(|table| table.remove(raw));
    drop(file);
  }
}

impl Table {
  fn index(raw: RawResource) -> Option<usize> {
    usize::try_from(raw.checked_sub(FIRST_ID)?).ok()
  }

  fn insert(&mut self, file: File) -> isize {
    let index = match self.free.pop() {
      Some(index) => {
        self.files[index] = Some(file);
        index
      }
      None => {
        self.files.push(Some(file));
        self.files.len() - 1
      }
    };
    // The table holds far fewer than `isize::MAX` entries.
    FIRST_ID as isize + index as isize
  }

  fn file(&mut self, raw: RawResource) -> Result<&mut File, i32> {
    Self::index(raw)
      .and_then(|index| self.files.get_mut(index))
      .and_then(Option::as_mut)
      .ok_or(libc::EBADF)
  }

  fn remove(&mut self, raw: RawResource) -> Option<File> {
    let index = Self::index(raw)?;
    let file = self.files.get_mut(index)?.take()?;
    self.free.push(index);
    Some(file)
  }

  fn preopens(&mut self) -> &[(Descriptor, String)] {
    self
      .preopens
      .get_or_insert_with(wasip3::filesystem::preopens::get_directories)
  }

  /// Resolves a path the way `openat` would: relative to `dir`, or, for an
  /// absolute path or the working directory, against the preopened
  /// directories. Returns the directory to call and the path below it.
  fn locate<'p>(
    &mut self,
    dir: RawResource,
    path: &'p str,
  ) -> Result<(&Descriptor, &'p str), i32> {
    if !path.starts_with('/') && dir != native::CWD {
      return Ok((&self.file(dir)?.descriptor, path));
    }
    let preopens = self.preopens();
    if let Some(below_root) = path.strip_prefix('/') {
      // The preopen with the longest name that is a whole-component prefix.
      let best = preopens
        .iter()
        .filter_map(|(descriptor, name)| {
          let name = name.trim_end_matches('/');
          let rest = path.strip_prefix(name)?;
          (rest.is_empty() || rest.starts_with('/')).then_some((
            descriptor,
            name.len(),
            rest,
          ))
        })
        .max_by_key(|(_, len, _)| *len);
      let Some((descriptor, _, rest)) = best else {
        return Err(libc::ENOENT);
      };
      let _ = below_root;
      let rest = rest.trim_start_matches('/');
      return Ok((descriptor, if rest.is_empty() { "." } else { rest }));
    }
    let cwd = wasip3::cli::environment::get_initial_cwd();
    let here = preopens
      .iter()
      .find(|(_, name)| name == "." || Some(name) == cwd.as_ref())
      .or_else(|| preopens.first());
    match here {
      Some((descriptor, _)) => Ok((descriptor, path)),
      None => Err(libc::ENOENT),
    }
  }
}

// ───────────────────────────── in-flight state ──────────────────────────────

/// Treats a borrow as lasting as long as the pending operation it is stored
/// in.
///
/// # Safety
///
/// The referent stays allocated and unmoved until the value built from the
/// result is dropped. For a path this is the `Op` in the same boxed
/// [`Pending`]; for a descriptor it is the table entry, kept open by the
/// `Resource` that `Op` holds.
unsafe fn held<T: ?Sized>(value: &T) -> &'static T {
  // SAFETY: guaranteed by the caller.
  unsafe { &*std::ptr::from_ref(value) }
}

/// One WASI call that completes the operation when it returns.
enum Call {
  Open { call: fs::OpenAt<'static>, append: bool },
  Stat(fs::Stat<'static>),
  StatAt(fs::StatAt<'static>),
  Mkdir(fs::CreateDirectoryAt<'static>),
  Unlink(fs::UnlinkFileAt<'static>),
  Rmdir(fs::RemoveDirectoryAt<'static>),
  Rename(fs::RenameAt<'static>),
  Link(fs::LinkAt<'static>),
  Symlink(fs::SymlinkAt<'static>),
  Readlink(fs::ReadlinkAt<'static>),
  Sync(fs::Sync<'static>),
}

impl Call {
  /// # Safety
  ///
  /// `self` stays allocated and unmoved until its completion is handled.
  unsafe fn push(&mut self, queue: &mut Queue, id: u64) {
    // SAFETY: guaranteed by the caller.
    unsafe {
      match self {
        Call::Open { call, .. } => queue.push_call(call, id),
        Call::Stat(call) => queue.push_call(call, id),
        Call::StatAt(call) => queue.push_call(call, id),
        Call::Mkdir(call) => queue.push_call(call, id),
        Call::Unlink(call) => queue.push_call(call, id),
        Call::Rmdir(call) => queue.push_call(call, id),
        Call::Rename(call) => queue.push_call(call, id),
        Call::Link(call) => queue.push_call(call, id),
        Call::Symlink(call) => queue.push_call(call, id),
        Call::Readlink(call) => queue.push_call(call, id),
        Call::Sync(call) => queue.push_call(call, id),
      }
    }
  }

  /// Turns the returned call's result into the operation's result, writing
  /// any output the operation asked for.
  fn finish(&mut self, op: &Op) -> isize {
    fn unit(result: Option<Result<(), ErrorCode>>) -> isize {
      match result {
        Some(Ok(())) => 0,
        Some(Err(error)) => fail(errno(&error)),
        None => unreachable!("a returned call has a result"),
      }
    }
    match self {
      Call::Open { call, append } => match call.result() {
        Some(Ok(descriptor)) => with_table(|table| {
          table.insert(File { descriptor, cursor: 0, append: *append })
        }),
        Some(Err(error)) => fail(errno(&error)),
        None => unreachable!("a returned call has a result"),
      },
      Call::Stat(call) => write_stat(op, call.result()),
      Call::StatAt(call) => write_stat(op, call.result()),
      Call::Readlink(call) => match call.result() {
        Some(Ok(target)) => {
          let Op::ReadlinkAt { buf, buf_len, .. } = op else {
            unreachable!("a readlink call belongs to a readlink operation")
          };
          let len = target.len().min(*buf_len);
          // SAFETY: the driver keeps `buf` valid for `buf_len` bytes until
          // this operation is reported, and `len` is within both.
          unsafe {
            std::ptr::copy_nonoverlapping(target.as_ptr(), buf.as_ptr(), len);
          }
          len as isize
        }
        Some(Err(error)) => fail(errno(&error)),
        None => unreachable!("a returned call has a result"),
      },
      Call::Mkdir(call) => unit(call.result()),
      Call::Unlink(call) => unit(call.result()),
      Call::Rmdir(call) => unit(call.result()),
      Call::Rename(call) => unit(call.result()),
      Call::Link(call) => unit(call.result()),
      Call::Symlink(call) => unit(call.result()),
      Call::Sync(call) => unit(call.result()),
    }
  }
}

fn write_stat(
  op: &Op,
  result: Option<Result<DescriptorStat, ErrorCode>>,
) -> isize {
  let Op::Stat { out, .. } = op else {
    unreachable!("a stat call belongs to a stat operation")
  };
  let stat = match result {
    Some(Ok(stat)) => stat,
    Some(Err(error)) => return fail(errno(&error)),
    None => unreachable!("a returned call has a result"),
  };
  let file_type = match stat.type_ {
    DescriptorType::RegularFile => FileType::File,
    DescriptorType::Directory => FileType::Directory,
    DescriptorType::SymbolicLink => FileType::Symlink,
    DescriptorType::BlockDevice => FileType::BlockDevice,
    DescriptorType::CharacterDevice => FileType::CharDevice,
    DescriptorType::Fifo => FileType::Fifo,
    DescriptorType::Socket => FileType::Socket,
    DescriptorType::Other(_) => FileType::Unknown,
  };
  // WASI reports no permissions or ownership.
  let stat = FileStat {
    file_type,
    size: stat.size,
    nlink: stat.link_count,
    ..FileStat::zeroed()
  };
  // SAFETY: the driver keeps `out` valid and unaliased until this operation
  // is reported.
  unsafe { out.as_ptr().write(stat) };
  0
}

/// Which stream a transfer moves bytes on.
#[derive(Clone, Copy)]
enum Stream {
  Read(ReadStream),
  Write(WriteStream),
}

/// What a transfer is waiting for.
enum Phase {
  /// A read or write of the current buffer.
  Data,
  /// The stream ended; its status says whether that was an error.
  Status,
  /// The data is written; the requested sync is running.
  Sync,
}

/// A read or write: bytes moved over a stream, buffer by buffer.
struct Transfer {
  /// The resource, to move its cursor or free its standard stream.
  raw: RawResource,
  stream: Stream,
  /// The stream belongs to this operation and is closed when it ends.
  /// A standard stream is shared and stays open.
  owned: bool,
  /// How the stream ended. Files have one; standard streams do not.
  status: Option<fs::StreamStatus>,
  /// A sync to run after the data is written.
  sync: Option<fs::Sync<'static>>,
  sync_data: Option<fs::SyncData<'static>>,
  phase: Phase,
  /// The buffer being filled or drained, and how far into it.
  index: usize,
  within: usize,
  total: usize,
  /// The operation used the file's cursor, which moves by `total`.
  moves_cursor: bool,
}

/// The `index`th buffer of a read or write.
fn buffer(op: &Op, index: usize) -> Option<(*mut u8, usize)> {
  fn span(buf: &impl ByteSpan) -> (*mut u8, usize) {
    (buf.bytes_ptr(), buf.bytes_len())
  }
  match op {
    Op::Read { iovecs, iov_count, .. } if index < *iov_count => {
      // SAFETY: the driver keeps `iov_count` buffers at `iovecs` valid
      // until this operation is reported.
      Some(span(unsafe { &*iovecs.as_ptr().add(index) }))
    }
    Op::Write { iovecs, iov_count, .. } if index < *iov_count => {
      // SAFETY: as above.
      Some(span(unsafe { &*iovecs.as_ptr().add(index) }))
    }
    _ => None,
  }
}

/// An operation the host has not finished.
struct Pending {
  /// Owns the paths the calls point into and keeps the resources open.
  op: Op,
  /// Keeps the operation's buffers allocated.
  _keep: Keep,
  stage: Stage,
}

enum Stage {
  /// Waiting for its standard stream to be free.
  Queued,
  Call(Call),
  Transfer(Transfer),
}

/// One of the component's standard streams. It carries one operation at a
/// time; the rest wait their turn.
#[derive(Default)]
struct Lane {
  stream: Option<Stream>,
  /// The future that reports how the stream ended. It is never read, but
  /// has to stay open as long as the stream does.
  _status: Option<Box<dyn Any>>,
  busy: bool,
  waiting: VecDeque<u64>,
}

/// The WASI 0.3 backend.
pub struct Wasi {
  queue: Queue,
  /// Pushed but not yet flushed.
  queued: Vec<(u64, Op, Keep)>,
  /// Boxed so that an operation's storage does not move while the host
  /// writes into it.
  ops: HashMap<u64, Box<Pending>>,
  /// Results waiting for the next `wait`.
  ready: Vec<OpCompleted>,
  /// Standard input, output and error, by resource id.
  lanes: [Lane; 3],
  timer: WaitFor,
  timer_pending: bool,
}

impl Wasi {
  pub fn new() -> Self {
    Self {
      queue: Queue::new(),
      queued: Vec::new(),
      ops: HashMap::new(),
      ready: Vec::new(),
      lanes: Default::default(),
      timer: WaitFor::new(Duration::ZERO),
      timer_pending: false,
    }
  }

  fn lane(raw: RawResource) -> Option<usize> {
    usize::try_from(raw).ok().filter(|index| *index < 3)
  }

  /// Starts an operation. It either finishes at once or joins `ops`.
  fn start(&mut self, id: u64, mut pending: Box<Pending>) {
    match self.begin(id, &mut pending) {
      Ok(()) => {
        self.ops.insert(id, pending);
      }
      Err(result) => self.ready.push(OpCompleted::new(id, result)),
    }
  }

  /// Starts `pending`. `Err` carries the result of an operation that is
  /// already finished, successfully or not.
  fn begin(&mut self, id: u64, pending: &mut Pending) -> Result<(), isize> {
    // SAFETY: `pending` is boxed and outlives every call built from `op`.
    let op: &'static Op = unsafe { held(&pending.op) };
    let call = match op {
      Op::Nop => return Err(0),
      Op::Read { fd, offset, .. } | Op::Write { fd, offset, .. } => {
        if !is_valid_file_offset(*offset) {
          return Err(fail(libc::EINVAL));
        }
        let raw = fd.as_raw_fd();
        if let Some(lane) = Self::lane(raw)
          && self.lanes[lane].busy
        {
          self.lanes[lane].waiting.push_back(id);
          pending.stage = Stage::Queued;
          return Ok(());
        }
        return self.begin_transfer(id, pending);
      }
      Op::OpenAt { dir_fd, path, flags, mode: _ } => {
        let (open_flags, descriptor_flags) = open_flags(*flags)?;
        let call = locate(dir_fd.as_raw_fd(), path, |dir, path| {
          fs::OpenAt::new(
            dir,
            PathFlags::SYMLINK_FOLLOW,
            path,
            open_flags,
            descriptor_flags,
          )
        })?;
        Call::Open { call, append: flags.contains(OpenFlags::APPEND) }
      }
      Op::Stat { target: StatTarget::Fd { fd }, .. } => {
        Call::Stat(descriptor(fd.as_raw_fd(), fs::Stat::new)?)
      }
      Op::Stat {
        target: StatTarget::Path { dir_fd, path, follow_symlinks },
        ..
      } => {
        let flags = if *follow_symlinks {
          PathFlags::SYMLINK_FOLLOW
        } else {
          PathFlags::empty()
        };
        Call::StatAt(locate(dir_fd.as_raw_fd(), path, |dir, path| {
          fs::StatAt::new(dir, flags, path)
        })?)
      }
      Op::MkdirAt { dir_fd, path, mode: _ } => Call::Mkdir(locate(
        dir_fd.as_raw_fd(),
        path,
        fs::CreateDirectoryAt::new,
      )?),
      Op::UnlinkAt { dir_fd, path, kind: UnlinkKind::File } => {
        Call::Unlink(locate(dir_fd.as_raw_fd(), path, fs::UnlinkFileAt::new)?)
      }
      Op::UnlinkAt { dir_fd, path, kind: UnlinkKind::Directory } => {
        Call::Rmdir(locate(
          dir_fd.as_raw_fd(),
          path,
          fs::RemoveDirectoryAt::new,
        )?)
      }
      Op::ReadlinkAt { dir_fd, path, .. } => {
        Call::Readlink(locate(dir_fd.as_raw_fd(), path, fs::ReadlinkAt::new)?)
      }
      Op::RenameAt { old_dir_fd, old_path, new_dir_fd, new_path } => {
        Call::Rename(locate2(
          (old_dir_fd.as_raw_fd(), old_path),
          (new_dir_fd.as_raw_fd(), new_path),
          fs::RenameAt::new,
        )?)
      }
      Op::LinkAt {
        kind: LinkKind::Hard,
        source_dir_fd,
        source_path,
        new_dir_fd,
        new_path,
      } => Call::Link(locate2(
        (source_dir_fd.as_raw_fd(), source_path),
        (new_dir_fd.as_raw_fd(), new_path),
        |old_dir, old_path, new_dir, new_path| {
          fs::LinkAt::new(
            old_dir,
            PathFlags::empty(),
            old_path,
            new_dir,
            new_path,
          )
        },
      )?),
      Op::LinkAt {
        kind: LinkKind::Soft,
        source_path,
        new_dir_fd,
        new_path,
        ..
      } => {
        let target = text(source_path)?;
        Call::Symlink(locate(new_dir_fd.as_raw_fd(), new_path, |dir, path| {
          fs::SymlinkAt::new(dir, target, path)
        })?)
      }
      Op::Fsync { fd } => {
        Call::Sync(descriptor(fd.as_raw_fd(), fs::Sync::new)?)
      }
      Op::GetCwd { out } => {
        let cwd = wasip3::cli::environment::get_initial_cwd()
          .unwrap_or_else(|| String::from("/"));
        // SAFETY: the driver keeps `out` valid and unaliased until this
        // operation is reported.
        unsafe { *out.as_ptr() = OsString::from(cwd) };
        return Err(0);
      }
      // Sockets, directory listing and processes are not implemented here.
      _ => return Err(fail(libc::ENOTSUP)),
    };
    pending.stage = Stage::Call(call);
    let Stage::Call(call) = &mut pending.stage else {
      unreachable!("the stage was just set")
    };
    // SAFETY: `pending` is boxed and stays in `ops` until the completion.
    unsafe { call.push(&mut self.queue, id) };
    Ok(())
  }

  /// Opens the stream for a read or write and starts its first buffer.
  fn begin_transfer(
    &mut self,
    id: u64,
    pending: &mut Pending,
  ) -> Result<(), isize> {
    let (raw, offset, writing, flags) = match &pending.op {
      Op::Read { fd, offset, .. } => {
        (fd.as_raw_fd(), *offset, false, WriteFlags::empty())
      }
      Op::Write { fd, offset, flags, .. } => {
        (fd.as_raw_fd(), *offset, true, *flags)
      }
      _ => unreachable!("only reads and writes transfer"),
    };

    let mut transfer = match Self::lane(raw) {
      Some(lane) => {
        if !flags.is_empty() {
          return Err(fail(libc::ENOTSUP));
        }
        let stream = self.standard_stream(lane, writing)?;
        self.lanes[lane].busy = true;
        Transfer {
          raw,
          stream,
          owned: false,
          status: None,
          sync: None,
          sync_data: None,
          phase: Phase::Data,
          index: 0,
          within: 0,
          total: 0,
          moves_cursor: false,
        }
      }
      None => {
        let append = flags.contains(WriteFlags::APPEND);
        let (stream, status, sync, sync_data) = with_table(|table| {
          let file = table.file(raw).map_err(fail)?;
          let at = offset.unwrap_or(file.cursor);
          // SAFETY: the `Resource` in `pending.op` keeps the file open
          // until these calls are dropped.
          let descriptor = unsafe { held(&file.descriptor) };
          let (stream, status) = if writing {
            let (writer, reader) = wasip3::wit_stream::new::<u8>();
            let status = if append || (offset.is_none() && file.append) {
              descriptor.append_via_stream(reader)
            } else {
              descriptor.write_via_stream(reader, at)
            };
            (Stream::Write(self.queue.add_writer(writer)), status)
          } else {
            let (reader, status) = descriptor.read_via_stream(at);
            (Stream::Read(self.queue.add_reader(reader)), status)
          };
          let sync =
            flags.contains(WriteFlags::SYNC).then(|| fs::Sync::new(descriptor));
          let sync_data = (flags.contains(WriteFlags::DSYNC) && sync.is_none())
            .then(|| fs::SyncData::new(descriptor));
          Ok::<_, isize>((stream, status, sync, sync_data))
        })?;
        Transfer {
          raw,
          stream,
          owned: true,
          status: Some(fs::StreamStatus::new(status)),
          sync,
          sync_data,
          phase: Phase::Data,
          index: 0,
          within: 0,
          total: 0,
          moves_cursor: offset.is_none(),
        }
      }
    };

    let more = self.push_data(id, &pending.op, &mut transfer);
    pending.stage = Stage::Transfer(transfer);
    if more {
      return Ok(());
    }
    // There was nothing to move. A write still has to close its stream and
    // learn its status.
    match self.end_of_data(id, pending) {
      Some(result) => Err(result),
      None => Ok(()),
    }
  }

  /// The shared stream of a standard resource, opened on first use.
  fn standard_stream(
    &mut self,
    lane: usize,
    writing: bool,
  ) -> Result<Stream, isize> {
    // Standard input is read-only; output and error are write-only.
    if writing == (lane == 0) {
      return Err(fail(libc::EBADF));
    }
    if let Some(stream) = self.lanes[lane].stream {
      return Ok(stream);
    }
    let (stream, status): (Stream, Box<dyn Any>) = if lane == 0 {
      let (reader, status) = wasip3::cli::stdin::read_via_stream();
      (Stream::Read(self.queue.add_reader(reader)), Box::new(status))
    } else {
      let (writer, reader) = wasip3::wit_stream::new::<u8>();
      let status: Box<dyn Any> = if lane == 1 {
        Box::new(wasip3::cli::stdout::write_via_stream(reader))
      } else {
        Box::new(wasip3::cli::stderr::write_via_stream(reader))
      };
      (Stream::Write(self.queue.add_writer(writer)), status)
    };
    self.lanes[lane].stream = Some(stream);
    self.lanes[lane]._status = Some(status);
    Ok(stream)
  }

  /// Starts moving the next buffer that has room or data left. Returns
  /// `false` when every buffer is done.
  fn push_data(&mut self, id: u64, op: &Op, transfer: &mut Transfer) -> bool {
    loop {
      let Some((ptr, len)) = buffer(op, transfer.index) else {
        return false;
      };
      if transfer.within == len {
        transfer.index += 1;
        transfer.within = 0;
        continue;
      }
      // SAFETY: `within < len`, so the range is inside the buffer, which
      // the driver keeps valid until this operation is reported.
      let (ptr, len) =
        unsafe { (ptr.add(transfer.within), len - transfer.within) };
      // SAFETY: as above; nothing else touches the buffer meanwhile.
      let pushed = unsafe {
        match transfer.stream {
          Stream::Read(stream) => self.queue.push_read(stream, ptr, len, id),
          Stream::Write(stream) => self.queue.push_write(stream, ptr, len, id),
        }
      };
      if pushed.is_err() {
        unreachable!("a transfer's stream is open and carries one operation")
      }
      return true;
    }
  }

  /// Handles a completion from the queue.
  fn on_completion(&mut self, done: Completion) {
    let id = done.user_data();
    if id == TIMER {
      self.timer_pending = false;
      return;
    }
    let Some(mut pending) = self.ops.remove(&id) else {
      unreachable!("a completion for an operation that is not in flight")
    };
    match self.advance(id, &mut pending, done.outcome()) {
      Some(result) => self.finish(id, pending, result),
      None => {
        self.ops.insert(id, pending);
      }
    }
  }

  /// Reports an operation and frees what it held.
  fn finish(&mut self, id: u64, pending: Box<Pending>, result: isize) {
    let lane = match &pending.stage {
      Stage::Transfer(transfer) => Self::lane(transfer.raw),
      _ => None,
    };
    self.ready.push(OpCompleted::new(id, result));
    drop(pending);
    close_deferred();
    if let Some(lane) = lane {
      self.lanes[lane].busy = false;
      self.start_waiting(lane);
    }
  }

  /// Starts the next operation waiting for a standard stream, if any.
  fn start_waiting(&mut self, lane: usize) {
    while !self.lanes[lane].busy {
      let Some(next) = self.lanes[lane].waiting.pop_front() else {
        return;
      };
      let Some(mut pending) = self.ops.remove(&next) else {
        unreachable!("a waiting operation is in `ops`")
      };
      match self.begin_transfer(next, &mut pending) {
        Ok(()) => {
          self.ops.insert(next, pending);
        }
        Err(result) => {
          self.lanes[lane].busy = false;
          self.ready.push(OpCompleted::new(next, result));
        }
      }
    }
  }

  /// Moves an operation forward after one of its steps completed. Returns
  /// its result once it is finished.
  fn advance(
    &mut self,
    id: u64,
    pending: &mut Pending,
    outcome: Outcome,
  ) -> Option<isize> {
    let Pending { op, stage, .. } = pending;
    match stage {
      Stage::Queued => unreachable!("a queued operation has nothing in flight"),
      Stage::Call(call) => Some(match outcome {
        Outcome::Returned => call.finish(op),
        Outcome::Cancelled(_) => CANCELLED,
        other => unreachable!("a call ended as {other:?}"),
      }),
      Stage::Transfer(transfer) => match transfer.phase {
        Phase::Data => {
          transfer.total += outcome.count();
          transfer.within += outcome.count();
          match outcome {
            // A file is read until the buffers are full or it ends. A
            // standard stream returns what arrived, like a pipe.
            Outcome::Transferred(_)
              if (transfer.owned
                || matches!(transfer.stream, Stream::Write(_)))
                && self.push_data(id, op, transfer) =>
            {
              None
            }
            Outcome::Transferred(_) => self.end_of_data(id, pending),
            Outcome::Closed(_) => self.stream_ended(id, pending),
            Outcome::Cancelled(_) => {
              let total = transfer.total;
              self.release(pending);
              Some(if total > 0 { total as isize } else { CANCELLED })
            }
            Outcome::Returned => unreachable!("a stream operation returned"),
          }
        }
        Phase::Status => {
          let status = match (&outcome, transfer.status.as_mut()) {
            (Outcome::Returned, Some(status)) => status.result(),
            _ => None,
          };
          match (outcome, status) {
            (Outcome::Returned, Some(Ok(()))) => self.after_status(id, pending),
            (Outcome::Returned, Some(Err(error))) => {
              self.release(pending);
              Some(fail(errno(&error)))
            }
            (Outcome::Cancelled(_), _) => {
              self.release(pending);
              Some(CANCELLED)
            }
            (other, _) => unreachable!("a stream status ended as {other:?}"),
          }
        }
        Phase::Sync => {
          let result = match outcome {
            Outcome::Returned => {
              let synced = match (&mut transfer.sync, &mut transfer.sync_data) {
                (Some(sync), _) => sync.result(),
                (None, Some(sync)) => sync.result(),
                (None, None) => unreachable!("a sync phase has a sync call"),
              };
              match synced {
                Some(Ok(())) => transfer.total as isize,
                Some(Err(error)) => fail(errno(&error)),
                None => unreachable!("a returned call has a result"),
              }
            }
            Outcome::Cancelled(_) => CANCELLED,
            other => unreachable!("a sync ended as {other:?}"),
          };
          self.release(pending);
          Some(result)
        }
      },
    }
  }

  /// Every buffer is done. A read is finished; a write closes its stream
  /// and waits for the status.
  fn end_of_data(&mut self, id: u64, pending: &mut Pending) -> Option<isize> {
    let Stage::Transfer(transfer) = &mut pending.stage else {
      unreachable!("only a transfer has data")
    };
    if transfer.owned && matches!(transfer.stream, Stream::Write(_)) {
      return self.stream_ended(id, pending);
    }
    let total = transfer.total;
    self.release(pending);
    Some(total as isize)
  }

  /// The stream is finished, by its end or by this side closing it. A file
  /// then reports whether that was an error.
  fn stream_ended(&mut self, id: u64, pending: &mut Pending) -> Option<isize> {
    let Stage::Transfer(transfer) = &mut pending.stage else {
      unreachable!("only a transfer has a stream")
    };
    self.close_stream(transfer);
    match &mut transfer.status {
      Some(status) => {
        transfer.phase = Phase::Status;
        // SAFETY: `pending` is boxed and stays in `ops` until the
        // completion.
        unsafe { self.queue.push_call(status, id) };
        None
      }
      None => {
        // A standard stream that ended: end of input, or a closed output.
        let total = transfer.total;
        let writing = matches!(transfer.stream, Stream::Write(_));
        self.release(pending);
        Some(if writing && total == 0 {
          fail(libc::EPIPE)
        } else {
          total as isize
        })
      }
    }
  }

  /// The stream ended without an error. Runs the sync a write asked for,
  /// or finishes.
  fn after_status(&mut self, id: u64, pending: &mut Pending) -> Option<isize> {
    let Stage::Transfer(transfer) = &mut pending.stage else {
      unreachable!("only a transfer has a status")
    };
    // SAFETY: `pending` is boxed and stays in `ops` until the completion.
    unsafe {
      match (&mut transfer.sync, &mut transfer.sync_data) {
        (Some(sync), _) => self.queue.push_call(sync, id),
        (None, Some(sync)) => self.queue.push_call(sync, id),
        (None, None) => {
          let total = transfer.total;
          self.release(pending);
          return Some(total as isize);
        }
      }
    }
    transfer.phase = Phase::Sync;
    None
  }

  /// Closes a stream that belongs to one operation.
  fn close_stream(&mut self, transfer: &mut Transfer) {
    if !std::mem::take(&mut transfer.owned) {
      return;
    }
    let closed = match transfer.stream {
      Stream::Read(stream) => self.queue.close_reader(stream),
      Stream::Write(stream) => self.queue.close_writer(stream),
    };
    if closed.is_err() {
      unreachable!("an owned stream is open until it is closed here")
    }
  }

  /// Ends a transfer: closes its stream and moves the file's cursor.
  fn release(&mut self, pending: &mut Pending) {
    let Stage::Transfer(transfer) = &mut pending.stage else {
      return;
    };
    let was_owned = transfer.owned || transfer.status.is_some();
    self.close_stream(transfer);
    if was_owned && transfer.moves_cursor {
      let (raw, total) = (transfer.raw, transfer.total as u64);
      with_table(|table| {
        if let Ok(file) = table.file(raw) {
          file.cursor += total;
        }
      });
    }
  }

  /// Handles every completion that is ready without blocking.
  fn pump(&mut self) {
    while let Some(done) = self.queue.poll() {
      self.on_completion(done);
    }
  }
}

impl Default for Wasi {
  fn default() -> Self {
    Self::new()
  }
}

/// The path as the UTF-8 string WASI requires.
fn text(path: &'static OsString) -> Result<&'static str, isize> {
  path.to_str().ok_or(fail(libc::EILSEQ))
}

/// Builds a call on the open file `raw`.
fn descriptor<C>(
  raw: RawResource,
  build: impl FnOnce(&'static Descriptor) -> C,
) -> Result<C, isize> {
  with_table(|table| {
    let file = table.file(raw).map_err(fail)?;
    // SAFETY: the `Resource` in the pending operation keeps the file open
    // until the call is dropped.
    Ok(build(unsafe { held(&file.descriptor) }))
  })
}

/// Builds a call on `path` resolved against `dir`.
fn locate<C>(
  dir: RawResource,
  path: &'static OsString,
  build: impl FnOnce(&'static Descriptor, &'static str) -> C,
) -> Result<C, isize> {
  let path = text(path)?;
  with_table(|table| {
    let (descriptor, path) = table.locate(dir, path).map_err(fail)?;
    // SAFETY: an open file is kept open by the `Resource` in the pending
    // operation; a preopened directory is never closed.
    Ok(build(unsafe { held(descriptor) }, path))
  })
}

/// Builds a call on two paths, each resolved against its own directory.
fn locate2<C>(
  (old_dir, old_path): (RawResource, &'static OsString),
  (new_dir, new_path): (RawResource, &'static OsString),
  build: impl FnOnce(
    &'static Descriptor,
    &'static str,
    &'static Descriptor,
    &'static str,
  ) -> C,
) -> Result<C, isize> {
  let (old_path, new_path) = (text(old_path)?, text(new_path)?);
  with_table(|table| {
    let (old, old_path) = table.locate(old_dir, old_path).map_err(fail)?;
    // SAFETY: as in `locate`.
    let old = unsafe { held(old) };
    let (new, new_path) = table.locate(new_dir, new_path).map_err(fail)?;
    // SAFETY: as in `locate`.
    Ok(build(old, old_path, unsafe { held(new) }, new_path))
  })
}

/// Translates portable open flags to WASI's two flag sets.
fn open_flags(
  flags: OpenFlags,
) -> Result<(fs::OpenFlags, DescriptorFlags), isize> {
  let mut open = fs::OpenFlags::empty();
  let mut descriptor = DescriptorFlags::empty();
  if flags.reads() {
    descriptor |= DescriptorFlags::READ;
  }
  if flags.writes() {
    descriptor |= DescriptorFlags::WRITE;
  }
  if flags.contains(OpenFlags::DIRECTORY) {
    // A directory is opened to be the anchor of later path operations,
    // which need permission to change its entries.
    open |= fs::OpenFlags::DIRECTORY;
    descriptor = DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY;
  }
  if descriptor.is_empty() {
    return Err(fail(libc::EINVAL));
  }
  if flags.contains(OpenFlags::CREATE) {
    open |= fs::OpenFlags::CREATE;
  }
  if flags.contains(OpenFlags::TRUNCATE) {
    open |= fs::OpenFlags::TRUNCATE;
  }
  if flags.contains(OpenFlags::EXCLUSIVE) {
    open |= fs::OpenFlags::EXCLUSIVE;
  }
  Ok((open, descriptor))
}

// SAFETY:
// - Each pushed operation's `Keep` lives in its `Pending` (or in `queued`)
//   until the operation is reported or drained.
// - An operation is reported once: when its last step completes, or with a
//   cancellation after the host confirmed it stopped. `lio_wasi::Queue`
//   cancels synchronously, so the host no longer uses the operation's
//   memory when the cancelled completion is handled.
// - Results are what the host returned. A new resource is a fresh table
//   entry, closed by `close_resource`.
// - Nothing runs between calls: a component has one thread, and the host
//   only writes while this backend waits.
unsafe impl IoBackend for Wasi {
  fn init(&mut self, cap: usize) -> io::Result<()> {
    self.queued.reserve(cap);
    self.ops.reserve(cap);
    self.ready.reserve(cap);
    Ok(())
  }

  fn resource_close(&self) -> ResourceClose {
    ResourceClose::with(close_resource)
  }

  fn push(&mut self, id: u64, op: InFlight<'_>, _step_bump: &mut Bump) {
    let (op, keep) = op.into_parts();
    self.queued.push((id, op, keep));
  }

  fn flush(&mut self) -> io::Result<()> {
    close_deferred();
    for (id, op, keep) in std::mem::take(&mut self.queued) {
      let pending = Box::new(Pending { op, _keep: keep, stage: Stage::Queued });
      self.start(id, pending);
    }
    Ok(())
  }

  fn cancel(&mut self, id: u64) {
    if let Some(at) = self.queued.iter().position(|(queued, ..)| *queued == id)
    {
      self.queued.swap_remove(at);
      self.ready.push(OpCompleted::new(id, CANCELLED));
      return;
    }
    let waiting = match self.ops.get(&id).map(|pending| &pending.stage) {
      Some(Stage::Queued) => true,
      Some(_) => false,
      None => return,
    };
    if waiting {
      for lane in &mut self.lanes {
        lane.waiting.retain(|queued| *queued != id);
      }
      self.ops.remove(&id);
      self.ready.push(OpCompleted::new(id, CANCELLED));
      close_deferred();
      return;
    }
    // The cancelled completion is queued and handled by the next `wait`.
    // If nothing was in flight, the operation's own completion is already
    // queued.
    self.queue.cancel(id);
  }

  fn drain(&mut self) -> bool {
    self.queued.clear();
    let ids: Vec<u64> = self.ops.keys().copied().collect();
    for id in ids {
      self.queue.cancel(id);
    }
    self.pump();
    // Whatever is left was waiting for a standard stream and never started.
    self.ops.clear();
    self.ready.clear();
    close_deferred();
    true
  }

  fn wait(
    &mut self,
    timeout: Option<Duration>,
    completed: &mut Vec<OpCompleted>,
  ) -> io::Result<()> {
    completed.clear();
    close_deferred();
    self.pump();
    if self.ready.is_empty() {
      match timeout {
        Some(Duration::ZERO) => {}
        None => {
          while self.ready.is_empty() {
            let Some(done) = self.queue.wait() else {
              break;
            };
            self.on_completion(done);
          }
        }
        Some(limit) => {
          self.timer = WaitFor::new(limit);
          self.timer_pending = true;
          // SAFETY: `self.timer` is not touched until `timer_pending` is
          // cleared by its completion, which happens before this returns.
          unsafe { self.queue.push_call(&mut self.timer, TIMER) };
          while self.ready.is_empty() && self.timer_pending {
            let Some(done) = self.queue.wait() else {
              break;
            };
            self.on_completion(done);
          }
          if self.timer_pending {
            self.queue.cancel(TIMER);
            self.pump();
          }
        }
      }
    }
    completed.append(&mut self.ready);
    Ok(())
  }
}
